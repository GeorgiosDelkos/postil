//! The run log: one [`Run`], its state in memory and its file on disk, and how the page sees
//! it ([`RunView`]). The runner (`run.rs`) creates runs, records what the agent reports, and
//! reads old runs back through this module.
//!
//! Each run is a file, `<git dir>/postil/runs/R<n>.jsonl`, written as the run goes: a header
//! line (which agent, which notes, the prompt), then one line per [`Entry`]. The owner chose
//! on 2026-11-23 to keep runs on disk, so a run survives a restart and scripts can read it.
//! The log is append-only: a tool call's later changes are their own entries, and the page
//! folds them into the call. A run id is claimed by creating its file, so two windows on one
//! working tree never share one.
//!
//! Each entry is one `write` to a file opened for appending, and nothing is synced: the log
//! is a record for the engineer, and a crash costs at most its last lines, while syncing on
//! every streamed message part would cost a disk flush each. After a failed write the log
//! is not written again, so a torn line can only be the last one, and the run says the log
//! is incomplete.
//!
//! The app never rewrites a log. It reads logs to show runs, and deletes one with the last of
//! its notes (`Runner::delete_runs_without_notes`). `runs/active.lock` also holds the highest
//! run number ever given, so a deleted run's number is not given again unless a crash loses
//! the counter (`last_run`). A log with no end is shown as unfinished: the app crashed or was
//! killed while it ran, or another window is running it. Closing the project or quitting ends
//! a run under way as failed (`Runner::close`).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, TryLockError};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::acp::{
    ContentBlock, Event, Location, PermissionKind, PlanEntryStatus, PromptUsage, SessionUpdate,
    StopReason, ToolCallContent, ToolCallStatus, ToolKind, TurnEnd, UsageCost,
};
use crate::analysis::AnalysisRun;
use crate::error::{Error, Result};
use crate::files::version_of;
use crate::report::Report;
use crate::review::{Diff, Finding, MemoryBlock, Stage, Work};
use crate::run::{FORCE_AFTER, lock};
use crate::usage::ledger::{self, LedgerRecord, LedgerSink, RECORD_VERSION, RunOrigin, RunOutcome};
use crate::usage::now_secs;

/// How long a send waits for the run lock or a thread's lock before it reports another
/// window's run ([`take_lock`]). Arbitrary: far longer than a fork-to-exec window, short
/// enough that a refused send answers at once.
const LOCK_PATIENCE: Duration = Duration::from_millis(200);

/// The most runs live at once in one repository, across windows (docs/design.md, "Keeping
/// the disk in bounds"). Arbitrary: the engineer reads every result, and more at once is more
/// than one person follows.
pub(super) const MAX_LIVE: usize = 3;

/// What a new run's header says before the run has an id, which [`Run::create`] gives it.
#[derive(Debug)]
pub(super) struct Opening<'a> {
    /// The agent's name, as the settings give it.
    pub(super) agent: &'a str,
    /// The notes the run carries: it goes with the last of them.
    pub(super) notes: Vec<String>,
    /// The prompt's text as it is built when the run is sent. The agent receives it without
    /// the line about team memory, or with it, when the run's start finds otherwise
    /// (`Runner::start`); a review's first stage receives it with the review's team memories,
    /// which its start recalls after this is logged (`Runner::recall_for_review`).
    pub(super) prompt: String,
    pub(super) origin: Origin,
    /// Whether the run's thread only reads ([`Header::read_only`]).
    pub(super) read_only: bool,
}

/// The thread a new run works in, whose lock it holds while live.
#[derive(Debug, Clone, Copy)]
pub(super) enum InThread<'a> {
    /// A new thread, whose id is the run's own.
    New,
    /// The open thread of this id.
    Open(&'a str),
}

/// The run log's format version, in its header line. Version 2 added a reviewer's run
/// (`review` in the header, the `findings` entry); version 3 a follow-up (`followUp` in the
/// header); version 4 the `snapshot` entry; version 5 an execute call's `command` and `exit`
/// on the `tool` and `toolUpdate` entries; version 6 the `report` entry; version 7 `readOnly`
/// in the header and the `undone` entry; version 8 an analysis run (`analysis` in the header,
/// the `noAnalysis` entry); version 9 a memory draft (`remember` in the header); version 10 a
/// finding's team memory (`memory` on a finding of the `findings` entry). Every older version
/// reads: a log before version 4 has no snapshots, one before version 5 no commands or exit
/// codes, one before version 6 no report, one before version 7 is a run that may change files,
/// one before version 8 is no analysis, one before version 9 no memory draft, and one before
/// version 10 no finding names a memory.
const LOG_VERSION: u32 = 10;

/// One run and its log.
pub(crate) struct Run {
    file: PathBuf,
    /// Where a summary record is appended once this run finishes, when configured.
    usage: Option<LedgerSink>,
    state: Mutex<RunState>,
}

/// What the agent reported about a live run's usage, kept in memory until the run ends.
#[derive(Debug, Default)]
struct RunUsage {
    /// The token counts of the prompt reply, which ends the turn.
    tokens: Option<PromptUsage>,
    /// What this turn alone cost, when the prompt reply reported it (Grok's `_meta`).
    direct_cost: Option<UsageCost>,
    context_used: Option<u64>,
    context_size: Option<u64>,
    /// The session's cumulative cost before this turn began (a kept agent on a follow-up).
    cost_baseline: Option<UsageCost>,
    /// The session's latest cumulative cost reported during this turn.
    cost: Option<UsageCost>,
}

impl RunUsage {
    fn record_update(&mut self, used: Option<u64>, size: Option<u64>, cost: Option<UsageCost>) {
        if let Some(used) = used {
            self.context_used = Some(used);
        }
        if let Some(size) = size {
            self.context_size = Some(size);
        }
        if let Some(cost) = cost.filter(valid_cost) {
            self.cost = Some(cost);
        }
    }

    fn record_turn_cost(&mut self, cost: Option<UsageCost>) {
        if let Some(cost) = cost.filter(valid_cost) {
            self.direct_cost = Some(cost);
        }
    }

    /// What this turn alone cost: the per-turn cost reported on the prompt reply when given,
    /// else the session's cumulative cost less `cost_baseline`. `None` when the cumulative
    /// figures cannot be compared (another currency, or a cumulative cost that went down, as
    /// a rounded or reset meter does): recording the whole session's cost would count the
    /// earlier turns twice.
    fn turn_cost(&self) -> Option<UsageCost> {
        if let Some(cost) = &self.direct_cost {
            return Some(cost.clone());
        }
        let cost = self.cost.as_ref()?;
        match &self.cost_baseline {
            None => Some(cost.clone()),
            Some(base) if base.currency == cost.currency && cost.amount >= base.amount => {
                Some(UsageCost {
                    amount: cost.amount - base.amount,
                    currency: cost.currency.clone(),
                })
            }
            Some(_) => None,
        }
    }

    /// Latest known cumulative session cost for a follow-up that reuses this session. When
    /// only per-turn costs (`direct_cost`) were reported, advances `cost_baseline` by this
    /// turn's cost so a later `usage_update` on the same session cannot count it again.
    fn session_cost(&self) -> Option<UsageCost> {
        if let Some(cost) = &self.cost {
            return Some(cost.clone());
        }
        match (&self.cost_baseline, &self.direct_cost) {
            (None, direct) => direct.clone(),
            (Some(base), None) => Some(base.clone()),
            (Some(base), Some(direct)) if base.currency == direct.currency => Some(UsageCost {
                amount: base.amount + direct.amount,
                currency: base.currency.clone(),
            }),
            (Some(_), Some(_)) => None,
        }
    }
}

fn valid_cost(cost: &UsageCost) -> bool {
    cost.amount.is_finite() && cost.amount >= 0.0 && !cost.currency.is_empty()
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is a fact of its own, set once by its own caller; no two make one state"
)]
pub(super) struct RunState {
    pub(super) header: Header,
    entries: Vec<Entry>,
    /// The log, open for appending while live; `None` after it ended or a write failed.
    log: Option<std::io::BufWriter<File>>,
    /// Reusable serialization buffer for `Run::write`, avoiding a `Vec<u8>` allocation per entry.
    write_buf: Vec<u8>,
    /// The exclusive lock on its thread's `threads/<id>.lock` ([`hold_thread_lock`]), held
    /// from the run's creation until it ends: whether it is held is whether the run is live
    /// (`RunState::live`). A log read from disk has none.
    lock: Option<File>,
    // Kept as each entry is recorded, so a status needs no scan of the entries.
    pub(super) started: bool,
    cancel_requested: bool,
    /// When this process asked the agent to stop, for [`FORCE_AFTER`]. Not logged.
    pub(super) cancel_asked: Option<Instant>,
    /// Set by the first caller that takes the snapshot after the run (the turn's end on the
    /// reader thread, a forced Stop, a close), so only it commits: two `git commit`s in one
    /// worktree at once would fail on its index lock. Not logged.
    pub(super) after_claimed: bool,
    /// Set by the first caller that ends the run while it holds the thread's lock for work
    /// after the agent's turn: an analysis run's merge (`run.rs`, `finish_analysis`), a forced
    /// Stop, or a close ([`Run::claim_end`]). The others leave the end to it: a forced Stop
    /// that logged the end during the merge would release the thread's lock, and the thread's
    /// worktree would be freed under the merge's reads. Not logged.
    end_claimed: bool,
    /// The notes sent, until the agent shows it took the turn; then they are marked sent.
    pub(super) unmarked: Option<Vec<String>>,
    /// The underlying model id the agent reported for its session, if any.
    model: Option<String>,
    /// Usage metrics reported by the agent during this run.
    usage: RunUsage,
    end: Option<Status>,
    // Sets: a cancel can log a request's answer before the request itself.
    requested: BTreeSet<usize>,
    answered: BTreeSet<usize>,
    /// Each tool call's kind as last reported, by call id. An update need not name its call's
    /// kind (Claude's adapter names it only on the call, Grok only on an update), and only an
    /// execute call's command is kept ([`entry_of`]).
    kinds: HashMap<String, ToolKind>,
    /// Why the log is incomplete, the notes were not marked sent, or a review stopped here.
    pub(super) problem: Option<String>,
    /// Why the agent started without team memory, when the run was the one to say so
    /// (`memory.rs`, "Agents"). Kept apart from `problem`, whose first text wins: set as the
    /// agent starts, it would hide every later one, and the page reads `problem` as the run's
    /// trouble. Not in the log, as `problem` is not.
    pub(super) memory_notice: Option<String>,
}

/// The first line of a run log.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Header {
    version: u32,
    /// `R1`, `R2`: the log's file name.
    pub(super) id: String,
    /// The agent's name in the settings.
    pub(super) agent: String,
    /// The ids of the notes sent.
    pub(super) notes: Vec<String>,
    /// The prompt's text as it was built when the run was sent; the agent may have received
    /// it with or without the line about team memory (`Runner::start`). A review's first stage
    /// also received the review's team memories, which this does not hold: they were recalled
    /// after it was logged (`Runner::recall_for_review`). The stages after it hold them.
    pub(super) prompt: String,
    /// Seconds since the Unix epoch.
    started_at: u64,
    /// Set when the run is a reviewer's (`review.rs`): which run it reviews, and its place in
    /// the chain. Absent for a run of notes, and in every log written before reviews.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) review: Option<Stage>,
    /// Set when the run is a follow-up: the run it follows and the engineer's words. Absent
    /// in every log written before follow-ups. Never set together with `review`
    /// ([`Origin`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) follow_up: Option<FollowUp>,
    /// Set when the run's thread only reads (`thread::Mode::Read`): an ask, or a follow-up of
    /// one. Such a run takes no snapshot and reads no report, and every tool call it asks for
    /// but a read is refused. Absent, as false, in every log written before version 7.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) read_only: bool,
    /// Set when the run is an analysis (`analysis.rs`): what it was asked, which says what its
    /// reply may change. Such a run carries no notes, is read-only, and is not listed with the
    /// runs of notes. Absent in every log written before version 8. Never set together with
    /// `review` or `follow_up` ([`Origin`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) analysis: Option<AnalysisRun>,
    /// Set when the run drafts a team memory (`Runner::draft`): the thread's latest turn it
    /// drafts after. Such a run is read-only, whatever its thread's mode, and is not listed with
    /// the runs of notes; its reply ends with the draft ([`RunView::draft`]). Absent in every log
    /// written before version 9. Never set together with `review`, `follow_up` or `analysis`
    /// ([`Origin`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) remember: Option<Remember>,
}

/// What a new run is: a send of notes, a reviewer's stage, a follow-up, an analysis, or a
/// memory draft. One value, so a header never holds two of them.
#[derive(Debug)]
pub(super) enum Origin {
    Notes,
    Review(Stage),
    FollowUp(FollowUp),
    Analysis(AnalysisRun),
    Remember(Remember),
}

/// A memory draft's link to its thread: `of` is the thread's latest turn when the draft was
/// asked for (`R1`, or its latest follow-up).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Remember {
    pub(crate) of: String,
}

/// A finished memory draft, as the Remember dialog reads it: the agent's `{summary, body}`, or
/// why there is none, which the dialog shows above the note's text it falls back to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub(crate) enum DraftView {
    Ready { summary: String, body: String },
    Failed { problem: String },
}

/// The block a draft's reply ends with. Other fields are passed over: an agent that adds one
/// still drafted.
#[derive(Deserialize)]
struct DraftBlock {
    summary: String,
    body: String,
}

/// A follow-up run's link to its thread: `of` is the run it follows, `text` what the engineer
/// wrote. The prompt holds more than `text`: a kept agent's ends with the reminder to report
/// (`report::ASK_AGAIN`), and a fresh agent's starts with the thread so far.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct FollowUp {
    pub(super) of: String,
    pub(super) text: String,
}

/// One thing that happened in a run: a line of its log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub(crate) enum Entry {
    /// The agent is up and has the prompt.
    Started,
    /// Part of the agent's reply. Consecutive parts with the same `message` (the agent's
    /// message id, when it sends one) form one message.
    Message {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// Part of the agent's reasoning.
    Thought { text: String },
    /// A tool call began.
    Tool {
        id: String,
        title: String,
        tool: ToolKind,
        status: ToolCallStatus,
        places: Vec<Place>,
        output: Vec<Output>,
        /// An execute call's command line ([`command_of`]). Version 5 on.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
        /// A finished execute call's exit code ([`exit_of`]). Version 5 on.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit: Option<i64>,
    },
    /// A tool call changed; only the fields that changed are set.
    ToolUpdate {
        id: String,
        title: Option<String>,
        tool: Option<ToolKind>,
        status: Option<ToolCallStatus>,
        places: Option<Vec<Place>>,
        output: Option<Vec<Output>>,
        /// As in [`Entry::Tool`]: set when this change reports it, and it replaces the call's.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
        /// As in [`Entry::Tool`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit: Option<i64>,
    },
    /// The agent's plan, whole: each one replaces the last.
    Plan { steps: Vec<Step> },
    /// The agent asks to make a tool call. `request` is the agent's number for it, counting
    /// on across the turns of a kept agent (`acp`, "Permission requests").
    Permission {
        request: usize,
        tool_call: String,
        title: Option<String>,
        options: Vec<Choice>,
    },
    /// A permission request was answered; `option` is `None` when the request closed without
    /// an answer: the run was stopped, or the agent withdrew it. A question closed that way is
    /// logged here too; an answered one is [`Entry::Reply`].
    Answer {
        request: usize,
        option: Option<String>,
    },
    /// The agent asks the engineer a question. `fields` is the form, a JSON Schema object
    /// whose properties are its fields, as the agent sent it; the page draws it.
    Question {
        request: usize,
        message: String,
        fields: Value,
        tool_call: Option<String>,
    },
    /// The engineer answered question `request`: the form's values, or `None` for a skip.
    Reply {
        request: usize,
        content: Option<serde_json::Map<String, Value>>,
    },
    /// The agent sent something the app could not read; the run goes on.
    Unreadable { problem: String },
    /// The engineer asked the agent to stop.
    CancelRequested,
    /// The turn ended.
    Ended { reason: StopReason },
    /// The run failed: the agent did not start, refused the prompt, or exited.
    Failed { error: String },
    /// A proposing reviewer's findings, read from its reply when its turn ended
    /// (`review::findings_in`); `problem` says why none could be read.
    Findings {
        findings: Vec<Finding>,
        problem: Option<String>,
    },
    /// The author's report, read from the reply of a run that is not a review stage when its
    /// turn ended well (`report::read`), logged with the run's end.
    Report { report: Report },
    /// Where a [`Entry::Report`] would be: why no report could be read.
    NoReport { problem: String },
    /// An analysis run's turn ended well and nothing was stored: `problem` says why (its reply
    /// was empty, or did not read, or the file could not be written). Logged with the run's
    /// end, and becomes the run's problem. A run whose analysis was stored logs nothing: its
    /// diagrams name it.
    NoAnalysis { problem: String },
    /// The run changed files it may only read, and Postil put the worktree back: `text` says
    /// so, and becomes the run's problem. Logged before the run's end, for a run whose end
    /// logs no findings (a proposer's that ended well says it there).
    Undone { text: String },
    /// A snapshot of the working tree, a git tree id ([`Repo::snapshot`]), taken before the
    /// agent gets the prompt or as its turn ends.
    ///
    /// [`Repo::snapshot`]: crate::repo::Repo::snapshot
    Snapshot { point: Point, tree: String },
}

/// When a snapshot was taken: before a run, or after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Point {
    Before,
    After,
}

/// What runs changed, as `GET /api/run/changes` gives it: the snapshots compared, and the
/// diff between them as `git diff` prints it. `after` is `None` when the files as they are
/// now stood in for it.
#[derive(Debug, Serialize)]
pub(crate) struct Changes {
    pub(super) before: String,
    pub(super) after: Option<String>,
    pub(super) patch: String,
}

/// A file a tool call touches: relative to the repository root when it is inside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Place {
    path: String,
    line: Option<u32>,
}

/// What a tool call produced that the run panel shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub(crate) enum Output {
    Text {
        text: String,
    },
    Diff {
        path: String,
        old_text: Option<String>,
        new_text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Step {
    content: String,
    status: PlanEntryStatus,
}

/// An answer to a permission request, as the agent offered it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Choice {
    id: String,
    name: String,
    kind: PermissionKind,
}

/// Where a run is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Status {
    /// The agent is starting.
    Starting,
    Running,
    /// A permission request is open.
    Waiting,
    /// The engineer cancelled and the agent has not stopped yet.
    Cancelling,
    Ended,
    Failed,
    /// The log has no end and this process does not run it: the app crashed or was killed
    /// while it ran, or another window is running it.
    Unfinished,
}

/// A run as the page gets it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunView {
    id: String,
    agent: String,
    /// The notes sent and the prompt, left out of a view from a later entry than the first:
    /// they never change, and the prompt quotes every note with its code, often most of a
    /// reply the page asks for many times a second (ui/run-store.js keeps them).
    #[serde(skip_serializing_if = "Option::is_none")]
    notes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<String>,
    started_at: u64,
    status: Status,
    /// The entries from the number the page asked for on.
    entries: Vec<Entry>,
    /// How many entries the run has; the page asks for the ones after it next time.
    total: usize,
    problem: Option<String>,
    /// Why the agent started without team memory, when the run says so (`memory.rs`,
    /// "Agents"). Apart from `problem`, which the page reads as the run's trouble.
    memory_notice: Option<String>,
    /// Milliseconds until a second Stop ends the agent, 0 once it would, while this process
    /// is stopping the run; the page offers Force stop from then on, after a reload too.
    force_in: Option<u64>,
    /// For a reviewer's run, which run it reviews and its place in the chain.
    review: Option<Stage>,
    /// For a follow-up, the run it follows and the engineer's words.
    follow_up: Option<FollowUp>,
    /// Whether the run's thread only reads ([`Header::read_only`]): the page offers no Apply,
    /// pull request or review for it.
    read_only: bool,
    /// For an analysis run, what it was asked ([`Header::analysis`]).
    analysis: Option<AnalysisRun>,
    /// For a memory draft, the turn it drafts after ([`Header::remember`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    remember: Option<Remember>,
    /// For a memory draft that is over, the draft or why there is none ([`draft_of`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    draft: Option<DraftView>,
}

impl Run {
    /// Creates the log for a new run in `dir`, with the next free id, takes the lock of the
    /// run's thread, `thread`, and writes the header, which `opening` gives. `runs/active.lock`
    /// is held only for this: it makes taking a number, counting the live runs and taking a
    /// thread's lock one step across windows, so no two windows both see a place under
    /// [`MAX_LIVE`].
    ///
    /// `opening` is called with the run's id once the run has its place, under
    /// `runs/active.lock`, before the log exists: an ask saves its note there, sent in this run,
    /// so an ask refused at the cap saves nothing, and every log on disk names notes saved
    /// before it (`Runner::delete_runs_without_notes`).
    ///
    /// # Errors
    ///
    /// [`Error::RunElsewhere`] when another window's run works in the thread, or another
    /// window holds a lock this takes past [`LOCK_PATIENCE`]; [`Error::TooManyRuns`] when
    /// [`MAX_LIVE`] runs are live in the repository; [`Error::BadRequest`] for a thread id
    /// that is not a run's; [`Error::ReadRun`] when the thread locks cannot be listed or
    /// opened; the error of `opening`; and [`Error::WriteRun`] when a file cannot be written.
    pub(super) fn create<'a>(
        dir: &Path,
        thread: InThread<'_>,
        usage: Option<LedgerSink>,
        opening: impl FnOnce(&str) -> Result<Opening<'a>>,
    ) -> Result<Self> {
        let error = |path: &Path| {
            let path = path.to_owned();
            move |source| Error::WriteRun { path, source }
        };
        let active_path = dir.join("active.lock");
        let mut active = hold_run_lock(dir)?;
        let own = match thread {
            InThread::Open(id) => Some(id),
            InThread::New => None,
        };
        let lock = own.map(|id| hold_thread_lock(dir, id)).transpose()?;
        // A lock released a moment ago can still look held ([`take_lock`]): a count at the cap
        // is taken again until it is under it, for up to [`LOCK_PATIENCE`] in all, which only
        // a send about to be refused waits.
        let deadline = Instant::now() + LOCK_PATIENCE;
        let live = loop {
            let live = live_threads(dir, own)?;
            if live.len() < MAX_LIVE || Instant::now() >= deadline {
                break live;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        if live.len() >= MAX_LIVE {
            return Err(Error::TooManyRuns { live });
        }
        // A deleted run's number is not given again (`last_run`): a page that read the old R5
        // would show a new R5 as the old one. Every window takes a number under
        // `runs/active.lock`, held here, and the highest log on disk counts, so the number is
        // free until the file below claims it.
        let used = last_run(&mut active).map_err(error(&active_path))?;
        let number = highest_run(dir).map_err(error(dir))?.max(used) + 1;
        let id = format!("R{number}");
        let Opening {
            agent,
            notes,
            prompt,
            origin,
            read_only,
        } = opening(&id)?;
        // `create_new` all the same: a file something other than Postil made meanwhile is
        // refused, never written over.
        let file = dir.join(format!("{id}.jsonl"));
        let mut log = File::options()
            .append(true)
            .create_new(true)
            .open(&file)
            .map_err(|source| Error::WriteRun {
                path: file.clone(),
                source,
            })?;
        let (mut review, mut follow_up, mut analysis, mut remember) = (None, None, None, None);
        match origin {
            Origin::Notes => {}
            Origin::Review(stage) => review = Some(stage),
            Origin::FollowUp(followed) => follow_up = Some(followed),
            Origin::Analysis(asked) => analysis = Some(asked),
            Origin::Remember(drafted) => remember = Some(drafted),
        }
        let header = Header {
            version: LOG_VERSION,
            id,
            agent: agent.to_owned(),
            notes,
            prompt,
            started_at: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |since| since.as_secs()),
            review,
            follow_up,
            read_only,
            analysis,
            remember,
        };
        if let Err(source) = log.write_all(&json_line(&header)) {
            // A log without its header would be skipped as unreadable on every restart.
            drop(fs::remove_file(&file));
            return Err(Error::WriteRun { path: file, source });
        }
        if let Err(source) = record_last_run(&mut active, number) {
            drop(fs::remove_file(&file));
            return Err(Error::WriteRun {
                path: active_path,
                source,
            });
        }
        // A new thread's id is its first run's, unused until now, so its lock is free.
        let lock = match lock {
            Some(lock) => lock,
            None => hold_thread_lock(dir, &header.id).inspect_err(|_| {
                drop(fs::remove_file(&file));
            })?,
        };
        Ok(Self {
            file,
            usage,
            state: Mutex::new(RunState::new(header, Some(log), Some(lock))),
        })
    }

    /// The newest readable run log of notes in `dir`, read as a run this process does not run;
    /// `None` when there is none. An analysis run and a memory draft are passed over: the Run
    /// tab lists runs of notes. A newer log that cannot be read is skipped, and the run shown
    /// says why.
    pub(super) fn latest(dir: &Path) -> Result<Option<Self>> {
        let mut numbers = match run_numbers(dir) {
            Ok(numbers) => numbers,
            Err(source) => {
                return Err(Error::ReadRun {
                    path: dir.to_owned(),
                    source,
                });
            }
        };
        numbers.sort_unstable_by(|a, b| b.cmp(a));
        let mut skipped = None;
        for number in numbers {
            let file = dir.join(format!("R{number}.jsonl"));
            // Empty: another window created it a moment ago and is writing its header.
            if fs::metadata(&file).is_ok_and(|meta| meta.len() == 0) {
                continue;
            }
            match Self::read_if(&file, |header| {
                header.analysis.is_none() && header.remember.is_none()
            }) {
                Ok(Some(run)) => {
                    if let Some(skipped) = skipped {
                        run.state().problem.get_or_insert(skipped);
                    }
                    return Ok(Some(run));
                }
                Ok(None) => {}
                Err(problem) => {
                    skipped.get_or_insert(format!(
                        "{} is not shown: {problem} (move it aside to hide this)",
                        file.display()
                    ));
                }
            }
        }
        Ok(None)
    }

    /// Reads a run log. Its lines are decoded lossily, so a character torn by a crash spoils
    /// one line, not the log. The error says why the header cannot be read.
    pub(super) fn read(file: &Path) -> std::result::Result<Self, String> {
        Self::read_if(file, |_| true).map(|run| run.expect("predicate always returns true"))
    }

    /// Reads a run log in a single pass when `keep(&header)` is true, or returns `Ok(None)`
    /// after reading only line 1 when false so skipped analysis/remember logs are not loaded
    /// whole into memory.
    fn read_if(
        file: &Path,
        keep: impl FnOnce(&Header) -> bool,
    ) -> std::result::Result<Option<Self>, String> {
        let opened = File::open(file).map_err(|error| format!("it cannot be read ({error})"))?;
        let mut reader = BufReader::new(opened);
        let mut first = Vec::new();
        reader
            .read_until(b'\n', &mut first)
            .map_err(|error| format!("it cannot be read ({error})"))?;
        if first.last() == Some(&b'\n') {
            first.pop();
        }
        let header = parse_header(&first)?;
        if !keep(&header) {
            return Ok(None);
        }

        let mut rest = Vec::new();
        reader
            .read_to_end(&mut rest)
            .map_err(|error| format!("it cannot be read ({error})"))?;
        let mut state = RunState::new(header, None, None);
        for (index, line) in rest.split(|&byte| byte == b'\n').enumerate() {
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<Entry>(&String::from_utf8_lossy(line)) {
                Ok(entry) => {
                    state.apply(&entry);
                    state.entries.push(entry);
                }
                // A torn last line, or a newer build's entry kind: the rest of the run is still
                // worth showing.
                Err(error) => {
                    state.problem.get_or_insert_with(|| {
                        format!(
                            "line {} of {} is unreadable: {error}",
                            index + 2,
                            file.display()
                        )
                    });
                }
            }
        }
        Ok(Some(Self {
            file: file.to_owned(),
            usage: None,
            state: Mutex::new(state),
        }))
    }

    pub(super) fn state(&self) -> MutexGuard<'_, RunState> {
        lock(&self.state)
    }

    /// Records the underlying model id reported by the agent's session, when known.
    pub(super) fn set_model(&self, model: Option<&str>) {
        if let Some(model) = model {
            self.state().model = Some(model.to_owned());
        }
    }

    /// Records the cumulative session cost already accounted for by earlier turns on a kept
    /// agent, so this run logs only its incremental cost.
    pub(super) fn set_cost_baseline(&self, baseline: Option<UsageCost>) {
        self.state().usage.cost_baseline = baseline;
    }

    /// The latest cumulative session cost reported on this run (or its baseline if none).
    pub(super) fn session_cost(&self) -> Option<UsageCost> {
        self.state().usage.session_cost()
    }

    /// Claims the run's end for the caller: true when the run is live and no one claimed it
    /// before, under one hold of the state ([`RunState::end_claimed`]).
    pub(super) fn claim_end(&self) -> bool {
        let mut state = self.state();
        let claimed = state.live() && !state.end_claimed;
        state.end_claimed |= claimed;
        claimed
    }

    /// Adds an entry to the run and its log. A run that ended takes no more entries.
    pub(super) fn record(&self, entry: Entry) {
        let finished = {
            let mut state = self.state();
            if !state.live() {
                return;
            }
            let outcome = self.write(&mut state, entry);
            outcome.and_then(|outcome| self.usage_record(&state, outcome))
        };
        self.write_usage(finished);
    }

    /// Records the snapshot at `point` unless the run has one there, checked under the same
    /// hold of the state: a forced stop and the agent's last event may both end a run.
    pub(super) fn record_snapshot(&self, point: Point, tree: String) {
        let mut state = self.state();
        if state.live() && state.snapshot(point).is_none() {
            self.write(&mut state, Entry::Snapshot { point, tree });
        }
    }

    /// Appends `entry` to the log and to `state`, which the caller holds. Returns how the run
    /// ended when `entry` ended it.
    fn write(&self, state: &mut RunState, entry: Entry) -> Option<RunOutcome> {
        let flush = !matches!(entry, Entry::Message { .. } | Entry::Thought { .. });
        if let Some(log) = &mut state.log {
            state.write_buf.clear();
            serde_json::to_writer(&mut state.write_buf, &entry)
                .expect("run log entries are plain JSON values");
            state.write_buf.push(b'\n');
            let result = log
                .write_all(&state.write_buf)
                .and_then(|()| if flush { log.flush() } else { Ok(()) });
            if state.write_buf.capacity() > 64 * 1024 {
                state.write_buf.shrink_to(64 * 1024);
            }
            if let Err(error) = result {
                state.log = None;
                state.problem.get_or_insert(format!(
                    "the run log {} could not be written, so this run will not be shown whole after \
                     a restart: {error}",
                    self.file.display()
                ));
            }
        }
        let outcome = state.apply(&entry);
        state.entries.push(entry);
        outcome
    }

    /// Records an agent's event, and returns the notes to mark sent now: the first sign that
    /// the agent took the turn (an update, a permission request, or a stop other than a
    /// refusal) releases them. A turn that fails or is refused at once leaves them open.
    /// `answer`, the event's auto-answer, is recorded with it, at once; `report`, the author's
    /// report of a turn that ended well, just before it.
    pub(super) fn record_event(
        &self,
        event: Event,
        report: Option<Entry>,
        answer: Option<Entry>,
        root: &Path,
    ) -> Option<Vec<String>> {
        // One hold of the state for the event, its report, its answer and the notes: a reader
        // sees all the entries or none, and a run that ended meanwhile (a forced stop, a close)
        // takes none of them, nor marks its notes sent. Taken first, since an update is read by
        // the kind of its call.
        let (notes, finished) = {
            let mut state = self.state();
            if !state.live() {
                return None;
            }
            let entry = match event {
                Event::Update(SessionUpdate::UsageUpdate { used, size, cost }) => {
                    state.usage.record_update(used, size, cost);
                    return None;
                }
                Event::Update(update) => entry_of(update, root, &state.kinds)?,
                Event::Permission(request) => Entry::Permission {
                    request: request.number,
                    tool_call: request.tool_call.tool_call_id,
                    title: request.tool_call.title,
                    options: request
                        .options
                        .into_iter()
                        .map(|option| Choice {
                            id: option.option_id,
                            name: option.name,
                            kind: option.kind,
                        })
                        .collect(),
                },
                Event::Question(question) => Entry::Question {
                    request: question.number,
                    message: question.message,
                    fields: question.schema,
                    tool_call: question.tool_call,
                },
                Event::Withdrawn { number } => Entry::Answer {
                    request: number,
                    option: None,
                },
                Event::TurnEnded(TurnEnd::Stopped {
                    reason,
                    usage,
                    cost,
                }) => {
                    if usage.is_some() {
                        state.usage.tokens = usage;
                    }
                    state.usage.record_turn_cost(cost);
                    Entry::Ended { reason }
                }
                Event::TurnEnded(TurnEnd::Failed(error)) => Entry::Failed { error },
                Event::Unreadable(problem) => Entry::Unreadable { problem },
                Event::Exited { stderr } => Entry::Failed {
                    error: Error::AgentExited { stderr }.to_string(),
                },
            };
            // A refusal is the model declining the work: its notes stay open, as for an error.
            let took_turn = !matches!(
                entry,
                Entry::Failed { .. }
                    | Entry::Unreadable { .. }
                    | Entry::Ended {
                        reason: StopReason::Refusal
                    }
            );
            let mut outcome = None;
            for entry in report.into_iter().chain([entry]).chain(answer) {
                // Every entry is written; only the first one that ends the run sets the outcome.
                let ended = self.write(&mut state, entry);
                outcome = outcome.or(ended);
            }
            let finished = outcome.and_then(|outcome| self.usage_record(&state, outcome));
            let notes = if took_turn {
                state.unmarked.take()
            } else {
                None
            };
            (notes, finished)
        };
        self.write_usage(finished);
        notes
    }

    fn usage_record(&self, state: &RunState, outcome: RunOutcome) -> Option<LedgerRecord> {
        let sink = self.usage.as_ref()?;
        let origin = if state.header.review.is_some() {
            RunOrigin::Review
        } else if state.header.follow_up.is_some() {
            RunOrigin::FollowUp
        } else if state.header.analysis.is_some() {
            RunOrigin::Analysis
        } else if state.header.remember.is_some() {
            RunOrigin::Remember
        } else {
            RunOrigin::Notes
        };
        Some(LedgerRecord {
            version: RECORD_VERSION,
            run_id: state.header.id.clone(),
            repo: sink.repo.clone(),
            agent: state.header.agent.clone(),
            model: state.model.clone(),
            origin,
            outcome,
            started_at: state.header.started_at,
            ended_at: now_secs().max(state.header.started_at),
            tokens: state.usage.tokens.unwrap_or_default(),
            context_used: state.usage.context_used,
            context_size: state.usage.context_size,
            cost: state.usage.turn_cost(),
            tool_calls: state.kinds.len() as u64,
            secondary_model: false,
        })
    }

    /// Appends the finished run's usage record to the ledger. A failure does not fail the
    /// run, which has ended, but becomes the run's problem, so the engineer learns why the
    /// Usage report is missing it.
    fn write_usage(&self, record: Option<LedgerRecord>) {
        let (Some(sink), Some(record)) = (&self.usage, record) else {
            return;
        };
        if let Err(error) = ledger::append(&sink.file, &record) {
            self.state().problem.get_or_insert(format!(
                "this run's usage was not recorded in {}: {error}",
                sink.file.display()
            ));
        }
    }

    /// The run as the page gets it, with the entries from number `after` on, and the notes
    /// and the prompt only when `after` is 0.
    pub(super) fn view(&self, after: usize) -> RunView {
        let state = self.state();
        let whole = after == 0;
        RunView {
            id: state.header.id.clone(),
            agent: state.header.agent.clone(),
            notes: whole.then(|| state.header.notes.clone()),
            prompt: whole.then(|| state.header.prompt.clone()),
            started_at: state.header.started_at,
            status: state.status(),
            entries: state.entries.get(after..).unwrap_or_default().to_vec(),
            total: state.entries.len(),
            problem: state.problem.clone(),
            memory_notice: state.memory_notice.clone(),
            force_in: state.cancel_asked.filter(|_| state.live()).map(|at| {
                u64::try_from(FORCE_AFTER.saturating_sub(at.elapsed()).as_millis())
                    .expect("at most FORCE_AFTER, 3 s")
            }),
            review: state.header.review.clone(),
            follow_up: state.header.follow_up.clone(),
            read_only: state.header.read_only,
            analysis: state.header.analysis.clone(),
            remember: state.header.remember.clone(),
            draft: draft_of(&state),
        }
    }
}

/// The draft of a memory draft run that is over ([`DraftView`]); `None` for any other run, and
/// for one still under way. A draft whose agent changed files is refused whatever its reply
/// says: what it read is not the thread's tree. Reads the reply from its end once.
fn draft_of(state: &RunState) -> Option<DraftView> {
    state.header.remember.as_ref()?;
    if state.live() {
        return None;
    }
    let failed = |problem: String| Some(DraftView::Failed { problem });
    let undone = state.entries.iter().find_map(|entry| match entry {
        Entry::Undone { text } => Some(text.clone()),
        _ => None,
    });
    if let Some(text) = undone {
        return failed(text);
    }
    let end = state.entries.iter().rev().find_map(|entry| match entry {
        Entry::Ended { reason } => Some(Ok(*reason)),
        Entry::Failed { error } => Some(Err(error.clone())),
        _ => None,
    });
    match end {
        Some(Ok(StopReason::EndTurn)) => {}
        Some(Ok(StopReason::Cancelled)) => {
            return failed("the draft was stopped before the agent finished it".to_owned());
        }
        Some(Ok(_)) => return failed("the agent's turn ended before it finished".to_owned()),
        Some(Err(error)) => return failed(format!("the draft run failed: {error}")),
        None => return failed("the draft run did not finish".to_owned()),
    }
    match crate::review::last_json_block::<DraftBlock>(&state.reply()) {
        Ok(DraftBlock { summary, body })
            if !summary.trim().is_empty() && !body.trim().is_empty() =>
        {
            Some(DraftView::Ready { summary, body })
        }
        Ok(_) => failed("the agent's draft has a blank summary or body".to_owned()),
        Err(problem) => failed(format!("the agent's reply holds no draft: {problem}")),
    }
}

// Debug output names the run and its state, never its prompt or entries: they quote note
// bodies and code, which `notes.rs` keeps out of Debug output too.
impl fmt::Debug for Run {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state();
        f.debug_struct("Run")
            .field("id", &state.header.id)
            .field("entries", &state.entries.len())
            .field("status", &state.status())
            .finish_non_exhaustive()
    }
}

impl RunState {
    fn new(header: Header, log: Option<File>, lock: Option<File>) -> Self {
        Self {
            header,
            entries: Vec::new(),
            log: log.map(std::io::BufWriter::new),
            write_buf: Vec::new(),
            lock,
            started: false,
            cancel_requested: false,
            cancel_asked: None,
            after_claimed: false,
            end_claimed: false,
            unmarked: None,
            model: None,
            usage: RunUsage::default(),
            end: None,
            requested: BTreeSet::new(),
            answered: BTreeSet::new(),
            kinds: HashMap::new(),
            problem: None,
            memory_notice: None,
        }
    }

    /// The agent's reply: its message parts joined, a blank line between messages.
    ///
    /// Folded as the page folds it (`ui/run.js`, `foldRun`; `ui/status.js`, `replyText`), so
    /// the two read the same text: a part joins the message before it when it has the same
    /// message id (or none) and no step was drawn between them, and each message is trimmed.
    /// A step drawn is a thought, a request, an unreadable message, a failure, or a tool call
    /// the run has not seen before; a repeated report of a known call changes its step in
    /// place. Without this, an agent with no message ids would run a report's fence on from the
    /// text before a tool call, and no block would start a line; and the block the report
    /// keeps would not be found in the page's text.
    pub(super) fn reply(&self) -> String {
        let mut messages: Vec<String> = Vec::new();
        // The id of the last thing drawn, when it is a message.
        let mut last: Option<&Option<String>> = None;
        let mut calls: HashSet<&str> = HashSet::new();
        for entry in &self.entries {
            match entry {
                Entry::Message { text, message } => match messages.last_mut() {
                    Some(open) if last == Some(message) => open.push_str(text),
                    _ => {
                        messages.push(text.clone());
                        last = Some(message);
                    }
                },
                Entry::Tool { id, .. } | Entry::ToolUpdate { id, .. } => {
                    if calls.insert(id) {
                        last = None;
                    }
                }
                Entry::Thought { .. }
                | Entry::Permission { .. }
                | Entry::Question { .. }
                | Entry::Unreadable { .. }
                | Entry::Failed { .. } => last = None,
                Entry::Started
                | Entry::Plan { .. }
                | Entry::Answer { .. }
                | Entry::Reply { .. }
                | Entry::CancelRequested
                | Entry::Ended { .. }
                | Entry::Findings { .. }
                | Entry::Report { .. }
                | Entry::NoReport { .. }
                | Entry::NoAnalysis { .. }
                | Entry::Undone { .. }
                | Entry::Snapshot { .. } => {}
            }
        }
        messages
            .iter()
            .map(|message| message.trim())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// The run's work as a reviewer reads it: its prompt, its reply, the diffs of its
    /// finished tool calls, each call's last report, in the order the calls were made, and
    /// the author's report when it gave one.
    pub(super) fn work(&self) -> Work {
        let calls = self.tool_calls();
        let diffs = calls
            .into_iter()
            .filter(|call| call.2)
            .flat_map(|call| call.1)
            .filter_map(|output| match output {
                Output::Diff {
                    path,
                    old_text,
                    new_text,
                } => Some(Diff {
                    path: path.clone(),
                    before: old_text.clone(),
                    after: new_text.clone(),
                }),
                Output::Text { .. } => None,
            })
            .collect();
        let report = self.entries.iter().rev().find_map(|entry| match entry {
            Entry::Report { report } => Some(report.clone()),
            _ => None,
        });
        Work {
            prompt: self.header.prompt.clone(),
            reply: self.reply(),
            diffs,
            report,
            memories: MemoryBlock::default(),
        }
    }

    /// Whether any completed tool call produced a diff, without cloning diff bodies or building
    /// the full reply.
    pub(super) fn has_completed_diffs(&self) -> bool {
        self.tool_calls()
            .into_iter()
            .filter(|call| call.2)
            .flat_map(|call| call.1)
            .any(|output| matches!(output, Output::Diff { .. }))
    }

    /// The file paths of diffs from completed tool calls, in call order, without cloning diff
    /// bodies or building the reply.
    pub(super) fn edited_paths(&self) -> Vec<String> {
        self.tool_calls()
            .into_iter()
            .filter(|call| call.2)
            .flat_map(|call| call.1)
            .filter_map(|output| match output {
                Output::Diff { path, .. } => Some(path.clone()),
                Output::Text { .. } => None,
            })
            .collect()
    }

    fn tool_calls(&self) -> Vec<(&str, &[Output], bool)> {
        let mut calls: Vec<(&str, &[Output], bool)> = Vec::new();
        let mut index_by_id: HashMap<&str, usize> = HashMap::new();
        for entry in &self.entries {
            match entry {
                Entry::Tool {
                    id, status, output, ..
                } => {
                    let done = *status == ToolCallStatus::Completed;
                    if let Some(&idx) = index_by_id.get(id.as_str()) {
                        calls[idx] = (id, output, done);
                    } else {
                        index_by_id.insert(id.as_str(), calls.len());
                        calls.push((id, output, done));
                    }
                }
                Entry::ToolUpdate {
                    id, status, output, ..
                } => {
                    if let Some(&idx) = index_by_id.get(id.as_str()) {
                        let call = &mut calls[idx];
                        if let Some(output) = output {
                            call.1 = output;
                        }
                        if let Some(status) = status {
                            call.2 = *status == ToolCallStatus::Completed;
                        }
                    }
                }
                _ => {}
            }
        }
        calls
    }

    /// Whether this process runs the run and it has not ended.
    pub(super) fn live(&self) -> bool {
        self.lock.is_some()
    }

    /// Keeps the fields that sum up the entries current with one more. Returns how the run
    /// ended when `entry` ended it.
    fn apply(&mut self, entry: &Entry) -> Option<RunOutcome> {
        match entry {
            Entry::Started => self.started = true,
            Entry::CancelRequested => self.cancel_requested = true,
            Entry::Permission { request, .. } | Entry::Question { request, .. } => {
                self.requested.insert(*request);
            }
            Entry::Answer { request, .. } | Entry::Reply { request, .. } => {
                self.answered.insert(*request);
            }
            Entry::Ended { reason } => {
                self.finish(Status::Ended);
                return Some(if matches!(reason, StopReason::Cancelled) {
                    RunOutcome::Cancelled
                } else {
                    RunOutcome::Ended
                });
            }
            Entry::Failed { .. } => {
                self.finish(Status::Failed);
                return Some(RunOutcome::Failed);
            }
            // The run is not failed: the worktree was put back, and what the agent said stands
            // (docs/review-assist-plan.md, "Corrections to the design", 3).
            Entry::Undone { text } => {
                self.problem.get_or_insert_with(|| text.clone());
            }
            Entry::NoAnalysis { problem } => {
                self.problem.get_or_insert_with(|| problem.clone());
            }
            Entry::Tool { id, tool, .. }
            | Entry::ToolUpdate {
                id,
                tool: Some(tool),
                ..
            } => {
                self.kinds.insert(id.clone(), *tool);
            }
            Entry::Message { .. }
            | Entry::Thought { .. }
            | Entry::ToolUpdate { tool: None, .. }
            | Entry::Plan { .. }
            | Entry::Unreadable { .. }
            | Entry::Findings { .. }
            | Entry::Report { .. }
            | Entry::NoReport { .. }
            | Entry::Snapshot { .. } => {}
        }
        None
    }

    fn finish(&mut self, end: Status) {
        self.end = Some(end);
        self.log = None;
        // Another window may start a run in the thread now, or end it.
        self.lock = None;
    }

    /// The tree of the run's snapshot at `point`, when one was taken.
    pub(super) fn snapshot(&self, point: Point) -> Option<&str> {
        self.entries.iter().find_map(|entry| match entry {
            Entry::Snapshot { point: at, tree } if *at == point => Some(tree.as_str()),
            _ => None,
        })
    }

    fn status(&self) -> Status {
        if let Some(end) = self.end {
            return end;
        }
        if !self.live() {
            return Status::Unfinished;
        }
        if !self.started {
            return Status::Starting;
        }
        if self.cancel_requested {
            return Status::Cancelling;
        }
        if self.requested.difference(&self.answered).next().is_some() {
            return Status::Waiting;
        }
        Status::Running
    }
}

/// Opens `runs/active.lock` in the runs directory `dir`, making both when missing, and takes
/// its exclusive lock, held until the returned file is dropped. [`Run::create`] holds it while
/// it takes a run number and its thread's lock, milliseconds.
///
/// # Errors
///
/// [`Error::RunElsewhere`] while another window holds it past [`LOCK_PATIENCE`], and
/// [`Error::WriteRun`] when the file cannot be made or locked.
fn hold_run_lock(dir: &Path) -> Result<File> {
    let error = |path: &Path| {
        let path = path.to_owned();
        move |source| Error::WriteRun { path, source }
    };
    fs::create_dir_all(dir).map_err(error(dir))?;
    let active_path = dir.join("active.lock");
    let active = File::options()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&active_path)
        .map_err(error(&active_path))?;
    take_lock(&active).map_err(|source| match source {
        TryLockError::WouldBlock => Error::RunElsewhere,
        TryLockError::Error(source) => Error::WriteRun {
            path: active_path.clone(),
            source,
        },
    })?;
    Ok(active)
}

/// Opens the lock file of thread `id`, `threads/<id>.lock` beside the runs directory `dir`,
/// making both when missing, and takes its exclusive lock, held until the returned file is
/// dropped. A live run holds it ([`Run::create`]); ending or forgetting the thread holds it so
/// that no run, in any window, works in the worktree meanwhile. This window's own live run in
/// the thread holds it too, through another open file, so it refuses then as well.
///
/// # Errors
///
/// [`Error::BadRequest`] for an id that is not a run's (the record's ids can be edited by
/// hand, and this one names a file); [`Error::RunElsewhere`] while a run holds it; and
/// [`Error::WriteRun`] when the file cannot be made or locked.
pub(super) fn hold_thread_lock(dir: &Path, id: &str) -> Result<File> {
    lock_thread_file(dir, id, true)
}

/// Non-blocking variant of [`hold_thread_lock`] for background eviction (`room::take_idle`),
/// returning [`Error::RunElsewhere`] immediately instead of sleeping for [`LOCK_PATIENCE`].
pub(super) fn try_hold_thread_lock(dir: &Path, id: &str) -> Result<File> {
    lock_thread_file(dir, id, false)
}

fn lock_thread_file(dir: &Path, id: &str, wait: bool) -> Result<File> {
    if run_number(id).is_none() {
        return Err(Error::BadRequest(format!(
            "thread id {id:?}: expected R and a number"
        )));
    }
    let threads = threads_dir(dir);
    fs::create_dir_all(&threads).map_err(|source| Error::WriteRun {
        path: threads.clone(),
        source,
    })?;
    let path = threads.join(format!("{id}.lock"));
    let file = File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|source| Error::WriteRun {
            path: path.clone(),
            source,
        })?;
    let locked = if wait {
        take_lock(&file)
    } else {
        file.try_lock()
    };
    locked.map_err(|source| match source {
        TryLockError::WouldBlock => Error::RunElsewhere,
        TryLockError::Error(source) => Error::WriteRun { path, source },
    })?;
    Ok(file)
}

/// Deletes thread `id`'s lock file, once no run will work in the thread again: it was
/// forgotten, or never made it into the record. The caller holds its lock, so no window can
/// lock the file being removed. Another window's count that lists the file as it goes reads it
/// as free ([`live_threads`]).
pub(super) fn remove_thread_lock(dir: &Path, id: &str) {
    if run_number(id).is_some() {
        // A file left behind is an empty file that locks nothing; nothing reads it.
        drop(fs::remove_file(threads_dir(dir).join(format!("{id}.lock"))));
    }
}

/// The ids of the threads whose lock is held now, in any window, this one's included
/// ([`live_threads`]), counted under `runs/active.lock` as every count is. A thread's id is its
/// first run's, so an analysis run, whose thread has no other, is live exactly while its id is
/// here.
///
/// # Errors
///
/// As [`hold_run_lock`] and [`live_threads`].
pub(super) fn held_threads(dir: &Path) -> Result<Vec<String>> {
    let _active = hold_run_lock(dir)?;
    live_threads(dir, None)
}

/// The ids of the threads whose lock a run holds now, in any window, `except` the one the
/// caller holds; sorted. Each lock is only tried, never waited for, so a lock released a moment
/// ago can read as held ([`Run::create`] counts again). Every count runs under
/// [`hold_run_lock`], so no other count holds a lock for a moment meanwhile. Ending or
/// forgetting a thread holds its lock too, and reads as a live run while it lasts: seconds for
/// an Apply on a large repository.
fn live_threads(dir: &Path, except: Option<&str>) -> Result<Vec<String>> {
    let threads = threads_dir(dir);
    let entries = match fs::read_dir(&threads) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(Error::ReadRun {
                path: threads,
                source,
            });
        }
    };
    let mut live = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| Error::ReadRun {
            path: threads.clone(),
            source,
        })?;
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".lock")) else {
            continue;
        };
        if run_number(id).is_none() || Some(id) == except {
            continue;
        }
        let file = match File::open(entry.path()) {
            Ok(file) => file,
            // Removed since the listing ([`remove_thread_lock`]): no run holds it.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(Error::ReadRun {
                    path: entry.path(),
                    source,
                });
            }
        };
        match file.try_lock() {
            // Dropping the file releases the lock it took.
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => live.push(id.to_owned()),
            Err(TryLockError::Error(source)) => {
                return Err(Error::ReadRun {
                    path: entry.path(),
                    source,
                });
            }
        }
    }
    live.sort_by_key(|id| run_number(id));
    Ok(live)
}

/// `<git dir>/postil/threads`, beside the runs directory `dir`: one lock file per thread.
fn threads_dir(dir: &Path) -> PathBuf {
    dir.with_file_name("threads")
}

/// Takes the exclusive lock on `file`, or reports that another holds it.
///
/// A `flock` lock belongs to the open file, and a process this app forks (git, an agent)
/// holds a copy of every open file from its fork until its exec closes them. A lock released
/// a moment ago can so look held for microseconds, which a send right after a run ends hit in
/// the tests. The lock is therefore tried for up to [`LOCK_PATIENCE`]; a window that really
/// runs an agent holds it far longer.
fn take_lock(file: &File) -> std::result::Result<(), TryLockError> {
    let deadline = Instant::now() + LOCK_PATIENCE;
    loop {
        match file.try_lock() {
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            result => return result,
        }
    }
}

/// The numbers of the run logs in `dir`; none when there is no `dir`.
pub(super) fn run_numbers(dir: &Path) -> io::Result<Vec<u64>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut numbers = Vec::new();
    for entry in entries {
        let name = entry?.file_name();
        let number = name
            .to_str()
            .and_then(|name| name.strip_suffix(".jsonl"))
            .and_then(run_number);
        numbers.extend(number);
    }
    Ok(numbers)
}

/// The number of the run id `R7`. Written back and compared, so `R07` or `R+7` is no id: it
/// would name another log than its own, or none.
pub(super) fn run_number(id: &str) -> Option<u64> {
    let rest = id.strip_prefix('R')?;
    if rest.is_empty()
        || (rest.len() > 1 && rest.starts_with('0'))
        || !rest.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    rest.parse().ok()
}

/// A run log's first line, its header. The version is read on its own first: a newer header
/// would otherwise fail on its first new field, with a message that hides the real reason.
fn parse_header(first: &[u8]) -> std::result::Result<Header, String> {
    let version = version_of(first)
        .map_err(|error| format!("its first line is not a run header ({error})"))?;
    if !(1..=LOG_VERSION).contains(&version) {
        return Err(format!(
            "it is version {version}, and this build of Postil reads versions 1 to {LOG_VERSION}"
        ));
    }
    let header: Header = serde_json::from_slice(first)
        .map_err(|error| format!("its first line is not a run header ({error})"))?;
    // `Origin` makes the four exclusive when Postil writes a header, and an analysis and a
    // memory draft read-only; this holds a hand edit to it.
    let origins = [
        header.review.is_some(),
        header.follow_up.is_some(),
        header.analysis.is_some(),
        header.remember.is_some(),
    ];
    if origins.into_iter().filter(|set| *set).count() > 1 {
        return Err(
            "its header is more than one of a review, a follow-up, an analysis and a \
                    memory draft"
                .to_owned(),
        );
    }
    if header.analysis.is_some() && !header.read_only {
        return Err("its header is an analysis that may change files".to_owned());
    }
    if let Some(remember) = &header.remember {
        if !header.read_only {
            return Err("its header is a memory draft that may change files".to_owned());
        }
        if run_number(&remember.of).is_none() {
            return Err(format!(
                "its header drafts a memory after {:?}, which is not a run id",
                remember.of
            ));
        }
    }
    Ok(header)
}

/// The header of the run log `file`, its first line only: a long run's log is not read whole.
pub(super) fn read_header(file: &Path) -> std::result::Result<Header, String> {
    let mut first = Vec::new();
    File::open(file)
        .map(BufReader::new)
        .and_then(|mut reader| reader.read_until(b'\n', &mut first))
        .map_err(|error| format!("it cannot be read ({error})"))?;
    // The line keeps its newline: JSON allows whitespace after the value.
    parse_header(&first)
}

/// The highest run number among the logs in `dir`; 0 when there is none.
fn highest_run(dir: &Path) -> io::Result<u64> {
    Ok(run_numbers(dir)?.into_iter().max().unwrap_or(0))
}

/// The highest run number ever given, kept as text in `runs/active.lock`, the file every new
/// run locks: logs are deleted with their notes, so the highest log left can be lower. 0 when
/// the file is new or holds no number (a build before this one left it empty, or a crash tore
/// the write). A crash can also leave an older, lower number. Either way the highest log left
/// then decides, and a deleted run above it can get its number again.
fn last_run(active: &mut File) -> io::Result<u64> {
    // Read as bytes: damage that is not UTF-8 is no number too, not an error that would stop
    // every run before `record_last_run` repairs the file.
    let mut bytes = Vec::new();
    active.seek(SeekFrom::Start(0))?;
    active.read_to_end(&mut bytes)?;
    Ok(std::str::from_utf8(&bytes)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0))
}

/// Records `number` as the highest run number given. The caller holds the file's lock.
/// Written over the old text, then cut to its length: a truncate first and a crash would
/// leave the file empty, and without the cut a damaged tail would stay and never parse
/// again. Not synced, like the logs.
fn record_last_run(active: &mut File, number: u64) -> io::Result<()> {
    let text = number.to_string();
    active.seek(SeekFrom::Start(0))?;
    active.write_all(text.as_bytes())?;
    active.set_len(text.len() as u64)
}

/// A session update as a log entry; `None` for what the run panel does not show. `kinds`
/// holds each call's kind as last reported, for an update that does not name it: an execute
/// call's command and exit code are kept, and no other call's.
fn entry_of(
    update: SessionUpdate,
    root: &Path,
    kinds: &HashMap<String, ToolKind>,
) -> Option<Entry> {
    let entry = match update {
        SessionUpdate::AgentMessageChunk {
            content: ContentBlock::Text { text },
            message_id,
        } => Entry::Message {
            text,
            message: message_id,
        },
        SessionUpdate::AgentThoughtChunk {
            content: ContentBlock::Text { text },
        } => Entry::Thought { text },
        SessionUpdate::ToolCall(call) => {
            let (command, exit) = if call.kind == ToolKind::Execute {
                (
                    call.raw_input.as_ref().and_then(command_of),
                    call.raw_output
                        .as_ref()
                        .and_then(|output| exit_of(call.status, output)),
                )
            } else {
                (None, None)
            };
            Entry::Tool {
                command,
                exit,
                id: call.tool_call_id,
                title: call.title,
                tool: call.kind,
                status: call.status,
                places: places(call.locations, root),
                output: outputs(call.content, root),
            }
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let kind = update
                .kind
                .or_else(|| kinds.get(&update.tool_call_id).copied());
            let (command, exit) = if kind == Some(ToolKind::Execute) {
                (
                    update.raw_input.as_ref().and_then(command_of),
                    update
                        .raw_output
                        .as_ref()
                        .zip(update.status)
                        .and_then(|(output, status)| exit_of(status, output)),
                )
            } else {
                (None, None)
            };
            Entry::ToolUpdate {
                command,
                exit,
                id: update.tool_call_id,
                title: update.title,
                tool: update.kind,
                status: update.status,
                places: update.locations.map(|locations| places(locations, root)),
                output: update.content.map(|content| outputs(content, root)),
            }
        }
        SessionUpdate::Plan { entries } => Entry::Plan {
            steps: entries
                .into_iter()
                .map(|entry| Step {
                    content: entry.content,
                    status: entry.status,
                })
                .collect(),
        },
        // An image or a link in a message, usage updates (kept in `RunState.usage` by
        // `record_event`, never shown), and updates the panel does not show.
        SessionUpdate::AgentMessageChunk { .. }
        | SessionUpdate::AgentThoughtChunk { .. }
        | SessionUpdate::UsageUpdate { .. }
        | SessionUpdate::Other => return None,
    };
    Some(entry)
}

/// The most bytes of a command the log keeps. A heredoc can be megabytes, and a log is read
/// whole. Arbitrary: far above a command line a person reads.
const MAX_COMMAND: usize = 4096;

/// The command line in an execute call's `rawInput`: its `command` field when that is a
/// string with more than blanks, cut to [`MAX_COMMAND`] bytes with a note of its length.
/// Claude's adapter and Grok both send a string there (seen 2026-11-25, through Postil's own
/// connection); neither was seen to send an array, so an array is not read.
fn command_of(raw_input: &Value) -> Option<String> {
    let command = raw_input.get("command")?.as_str()?;
    if command.trim().is_empty() {
        return None;
    }
    if command.len() <= MAX_COMMAND {
        return Some(command.to_owned());
    }
    let kept = &command[..command.floor_char_boundary(MAX_COMMAND)];
    Some(format!("{kept}… ({} bytes in all)", command.len()))
}

/// The exit code in the `rawOutput` of a report that says the execute call finished (`status`
/// is that report's own: both agents send the end and the output together, seen 2026-11-25):
/// - Grok's object has `exit_code`. Grok ends a command that exits non-zero as `completed`, so
///   the code is the only sign it failed. Its updates while the command runs carry
///   `exit_code` 0 too, so a call still running has none.
/// - Claude's adapter ends such a command as `failed`, with text whose first line is
///   `Exit code N`; a command that succeeds gets its bare output. That text is read only from
///   a failed call: a command that succeeds can print the same words.
fn exit_of(status: ToolCallStatus, raw_output: &Value) -> Option<i64> {
    match status {
        ToolCallStatus::Completed | ToolCallStatus::Failed => {}
        ToolCallStatus::Pending | ToolCallStatus::InProgress | ToolCallStatus::Other => {
            return None;
        }
    }
    if let Some(code) = raw_output.get("exit_code") {
        return code.as_i64();
    }
    if status != ToolCallStatus::Failed {
        return None;
    }
    let text = raw_output.as_str()?.strip_prefix("Exit code ")?;
    text.lines().next()?.parse().ok()
}

fn places(locations: Vec<Location>, root: &Path) -> Vec<Place> {
    locations
        .into_iter()
        .map(|location| Place {
            path: relative(&location.path, root),
            line: location.line,
        })
        .collect()
}

fn outputs(content: Vec<ToolCallContent>, root: &Path) -> Vec<Output> {
    content
        .into_iter()
        .filter_map(|content| match content {
            ToolCallContent::Content {
                content: ContentBlock::Text { text },
            } => Some(Output::Text { text }),
            ToolCallContent::Diff {
                path,
                old_text,
                new_text,
            } => Some(Output::Diff {
                path: relative(&path, root),
                old_text,
                new_text,
            }),
            ToolCallContent::Content { .. } | ToolCallContent::Other => None,
        })
        .collect()
}

/// `path` relative to `root` when it is inside it; agents report absolute paths.
fn relative(path: &str, root: &Path) -> String {
    Path::new(path).strip_prefix(root).map_or_else(
        |_| path.to_owned(),
        |inside| inside.to_string_lossy().into_owned(),
    )
}

fn json_line<T: Serialize>(value: &T) -> Vec<u8> {
    let mut line = serde_json::to_vec(value).expect("run entries are plain data");
    line.push(b'\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a run of note N1 by agent "Fake" opens with.
    fn opening(prompt: &str) -> Opening<'static> {
        Opening {
            agent: "Fake",
            notes: vec!["N1".to_owned()],
            prompt: prompt.to_owned(),
            origin: Origin::Notes,
            read_only: false,
        }
    }

    /// A run of note N1 by agent "Fake", in a new thread, in the runs directory `dir`.
    fn create(dir: &Path, prompt: &str) -> Run {
        Run::create(dir, InThread::New, None, |_| Ok(opening(prompt))).unwrap()
    }

    #[test]
    fn debug_output_names_the_run_but_never_shows_its_prompt() {
        let dir = std::env::temp_dir()
            .join(format!("postil-run-debug-{}", std::process::id()))
            .join("runs");
        fs::create_dir_all(&dir).unwrap();
        let run = create(&dir, "secret prompt");
        run.record(Entry::Thought {
            text: "secret thought".to_owned(),
        });
        let debug = format!("{run:?}");
        assert!(
            debug.contains("R1") && debug.contains("entries: 1"),
            "{debug}"
        );
        assert!(!debug.contains("secret"), "{debug}");
        drop(run);
        drop(fs::remove_dir_all(dir.parent().unwrap()));
    }

    #[test]
    fn after_a_failed_write_the_log_is_not_written_again_and_the_run_says_so() {
        let dir = std::env::temp_dir()
            .join(format!("postil-run-log-{}", std::process::id()))
            .join("runs");
        fs::create_dir_all(&dir).unwrap();
        let run = create(&dir, "p");
        let written = fs::read(&run.file).unwrap();
        // A log that refuses writes, as a full disk would.
        run.state().log = Some(std::io::BufWriter::new(File::open(&run.file).unwrap()));

        run.record(Entry::Started);
        run.record(Entry::Thought {
            text: "x".to_owned(),
        });
        let state = run.state();
        assert!(state.log.is_none(), "dropped after the first failure");
        assert!(
            state
                .problem
                .as_deref()
                .unwrap()
                .contains("could not be written")
        );
        assert_eq!(state.entries.len(), 2, "the run itself goes on");
        assert!(state.live());
        assert_eq!(
            fs::read(&run.file).unwrap(),
            written,
            "nothing was appended"
        );
        drop(state);
        drop(fs::remove_dir_all(dir.parent().unwrap()));
    }

    /// A run like [`create`]'s whose usage record goes to `ledger`.
    fn create_with_ledger(dir: &Path, ledger: &Path) -> Run {
        let sink = LedgerSink {
            file: ledger.to_owned(),
            repo: "/repo".to_owned(),
        };
        Run::create(dir, InThread::New, Some(sink), |_| Ok(opening("p"))).unwrap()
    }

    #[test]
    fn a_cancelled_run_is_recorded_as_cancelled_once() {
        let temp = crate::testing::TempDir::new("run-ledger");
        let runs = temp.path().join("runs");
        fs::create_dir_all(&runs).unwrap();
        let file = temp.path().join("usage.jsonl");
        let run = create_with_ledger(&runs, &file);

        run.record(Entry::Ended {
            reason: StopReason::Cancelled,
        });
        // A second end (a forced stop racing the agent's own) records nothing more.
        run.record(Entry::Failed {
            error: "late".to_owned(),
        });

        let records = ledger::read(&file).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outcome, RunOutcome::Cancelled);
        assert_eq!(records[0].origin, RunOrigin::Notes);
    }

    #[test]
    fn a_usage_record_that_cannot_be_written_becomes_the_run_s_problem() {
        let temp = crate::testing::TempDir::new("run-ledger-refused");
        let runs = temp.path().join("runs");
        fs::create_dir_all(&runs).unwrap();
        // A directory where the ledger file should be: opening it for appending fails.
        let file = temp.path().join("usage.jsonl");
        fs::create_dir_all(&file).unwrap();
        let run = create_with_ledger(&runs, &file);

        run.record(Entry::Failed {
            error: "agent exited".to_owned(),
        });

        let problem = run
            .state()
            .problem
            .clone()
            .expect("the refusal is reported");
        assert!(problem.contains("usage was not recorded"), "{problem}");
    }

    #[test]
    fn a_turn_s_cost_is_the_session_cost_less_what_earlier_turns_cost() {
        let usd = |amount| UsageCost {
            amount,
            currency: "USD".to_owned(),
        };
        let turn = |baseline, cost| {
            RunUsage {
                cost_baseline: baseline,
                cost: Some(cost),
                ..RunUsage::default()
            }
            .turn_cost()
            .map(|cost| cost.amount)
        };
        assert_eq!(turn(None, usd(1.5)), Some(1.5));
        assert_eq!(turn(Some(usd(1.0)), usd(1.5)), Some(0.5));
        // A meter that went down cannot say what this turn cost.
        assert_eq!(turn(Some(usd(1.2)), usd(1.0)), None);
        let euros = UsageCost {
            amount: 2.0,
            currency: "EUR".to_owned(),
        };
        assert_eq!(turn(Some(usd(1.0)), euros), None);

        // A per-turn cost reported on the prompt reply stands on its own, even on a kept
        // session whose earlier turn cost more, and advances the session baseline.
        let mut usage = RunUsage {
            cost_baseline: Some(usd(0.05)),
            ..RunUsage::default()
        };
        usage.record_turn_cost(Some(usd(0.02)));
        assert_eq!(usage.turn_cost().map(|c| c.amount), Some(0.02));
        assert!((usage.session_cost().unwrap().amount - 0.07).abs() < 1e-12);
        assert_eq!(
            RunUsage {
                direct_cost: Some(usd(0.03)),
                ..RunUsage::default()
            }
            .session_cost()
            .map(|c| c.amount),
            Some(0.03)
        );
        assert_eq!(
            RunUsage {
                cost_baseline: Some(usd(0.05)),
                direct_cost: Some(UsageCost {
                    amount: 0.02,
                    currency: "EUR".to_owned(),
                }),
                ..RunUsage::default()
            }
            .session_cost(),
            None
        );

        // Zero cost is valid; negative, non-finite, or currency-less costs are ignored.
        let mut bounds = RunUsage::default();
        bounds.record_update(None, None, Some(usd(0.0)));
        assert_eq!(bounds.turn_cost().map(|c| c.amount), Some(0.0));
        for bad in [
            usd(-0.01),
            usd(f64::NAN),
            usd(f64::INFINITY),
            usd(f64::NEG_INFINITY),
            UsageCost {
                amount: 1.0,
                currency: String::new(),
            },
        ] {
            let mut u = RunUsage::default();
            u.record_update(None, None, Some(bad.clone()));
            u.record_turn_cost(Some(bad));
            assert_eq!(u.turn_cost(), None);
        }
    }

    #[test]
    fn the_work_holds_each_finished_call_s_last_diffs_and_the_reply_by_message() {
        let dir = std::env::temp_dir()
            .join(format!("postil-run-work-{}", std::process::id()))
            .join("runs");
        fs::create_dir_all(&dir).unwrap();
        let run = create(&dir, "p");
        let diff = |after: &str| Output::Diff {
            path: "a.rs".to_owned(),
            old_text: None,
            new_text: after.to_owned(),
        };
        let tool = |id: &str, status, output| Entry::Tool {
            id: id.to_owned(),
            title: "Edit".to_owned(),
            tool: ToolKind::Edit,
            status,
            places: Vec::new(),
            output,
            command: None,
            exit: None,
        };
        let message = |text: &str, id: &str| Entry::Message {
            text: text.to_owned(),
            message: Some(id.to_owned()),
        };
        let unnamed = |text: &str| Entry::Message {
            text: text.to_owned(),
            message: None,
        };
        for entry in [
            tool("t1", ToolCallStatus::Pending, Vec::new()),
            tool("t2", ToolCallStatus::Completed, vec![diff("second")]),
            Entry::ToolUpdate {
                id: "t1".to_owned(),
                title: None,
                tool: None,
                status: Some(ToolCallStatus::Completed),
                places: None,
                output: Some(vec![diff("first")]),
                command: None,
                exit: None,
            },
            message("one ", "m1"),
            message("part", "m1"),
            message("two", "m2"),
            // An agent that sends no message ids: parts in a row are one message, and a step
            // between two parts ends it.
            unnamed("three "),
            unnamed("joined"),
            tool("t3", ToolCallStatus::Completed, Vec::new()),
            unnamed("```json\n"),
            // A known call reported again changes its step in place: the message goes on.
            tool("t3", ToolCallStatus::Completed, Vec::new()),
            unnamed("{}\n```  "),
        ] {
            run.record(entry);
        }

        let work = run.state().work();
        let afters: Vec<_> = work.diffs.iter().map(|diff| diff.after.as_str()).collect();
        assert_eq!(afters, ["first", "second"], "the update reached t1 only");
        assert_eq!(
            work.reply, "one part\n\ntwo\n\nthree joined\n\n```json\n{}\n```",
            "each message trimmed, as the page shows it"
        );
        drop(run);
        drop(fs::remove_dir_all(dir.parent().unwrap()));
    }

    #[test]
    fn a_snapshot_point_is_recorded_once_and_only_while_the_run_is_live() {
        let dir = std::env::temp_dir()
            .join(format!("postil-run-snap-{}", std::process::id()))
            .join("runs");
        fs::create_dir_all(&dir).unwrap();
        let run = create(&dir, "p");
        let points = |run: &Run| {
            run.state()
                .entries
                .iter()
                .filter(|entry| matches!(entry, Entry::Snapshot { .. }))
                .count()
        };
        // A forced stop and the agent's last event may both end a run: one snapshot after.
        run.record_snapshot(Point::After, "a1".to_owned());
        run.record_snapshot(Point::After, "a2".to_owned());
        assert_eq!(points(&run), 1);
        assert_eq!(run.state().snapshot(Point::After), Some("a1"));
        run.record(Entry::Ended {
            reason: StopReason::EndTurn,
        });
        run.record_snapshot(Point::Before, "b".to_owned());
        assert_eq!(points(&run), 1, "an ended run takes no snapshot");
        drop(run);
        drop(fs::remove_dir_all(dir.parent().unwrap()));
    }

    /// Version 10 adds a finding's memory: a log of 8 or 9 reads with none, a finding without
    /// one is written as before, one with it reads back, and version 11 is refused.
    #[test]
    fn a_finding_s_memory_is_written_from_version_10_and_older_logs_read_without() {
        let dir = std::env::temp_dir()
            .join(format!("postil-run-memory-{}", std::process::id()))
            .join("runs");
        fs::create_dir_all(&dir).unwrap();
        let finding = |memory: Option<&str>| Finding {
            id: "F1".to_owned(),
            note: None,
            path: None,
            line: None,
            text: "eager".to_owned(),
            fix: None,
            memory: memory.map(str::to_owned),
        };
        let run = create(&dir, "p");
        run.record(Entry::Findings {
            findings: vec![finding(Some("mem_01A")), finding(None)],
            problem: None,
        });
        drop(run);
        let file = dir.join("R1.jsonl");
        let written = fs::read_to_string(&file).unwrap();
        let (head, entries) = written.split_once('\n').unwrap();
        assert!(head.starts_with(r#"{"version":10,"#), "{head}");
        assert!(
            entries.contains(r#""fix":null,"memory":"mem_01A"},{"id":"F1""#),
            "{entries}"
        );
        assert!(
            entries.contains(r#""fix":null}]"#),
            "no memory, no field: {entries}"
        );
        let read = |file: &Path| -> Vec<Option<String>> {
            Run::read(file)
                .unwrap()
                .state()
                .entries
                .iter()
                .find_map(|entry| match entry {
                    Entry::Findings { findings, .. } => {
                        Some(findings.iter().map(|f| f.memory.clone()).collect())
                    }
                    _ => None,
                })
                .unwrap()
        };
        assert_eq!(read(&file), [Some("mem_01A".to_owned()), None]);

        let older = |version: u32| {
            let line = concat!(
                r#"{"kind":"findings","findings":[{"id":"F1","note":null,"path":null,"#,
                r#""line":null,"text":"x","fix":null}],"problem":null}"#,
            );
            let head = head.replace(r#""version":10"#, &format!(r#""version":{version}"#));
            let file = dir.join(format!("V{version}.jsonl"));
            fs::write(&file, format!("{head}\n{line}\n")).unwrap();
            file
        };
        for version in [8, 9] {
            assert_eq!(read(&older(version)), [None], "version {version}");
        }
        let problem = Run::read(&older(11)).unwrap_err();
        assert!(
            problem.contains("it is version 11, and this build of Postil reads versions 1 to 10"),
            "{problem}"
        );
        drop(fs::remove_dir_all(dir.parent().unwrap()));
    }
}
