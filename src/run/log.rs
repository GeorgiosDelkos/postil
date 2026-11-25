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

use std::collections::{BTreeSet, HashMap};
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
use crate::error::{Error, Result};
use crate::files::version_of;
use crate::review::{Diff, Finding, Stage, Work};
use crate::run::{FORCE_AFTER, lock};
use crate::usage::{
    ledger::{self, LedgerRecord, LedgerSink, RECORD_VERSION},
    now_secs,
};

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
    pub(super) notes: &'a [String],
    /// The prompt's text, as the agent receives it.
    pub(super) prompt: &'a str,
    pub(super) origin: Origin,
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
/// on the `tool` and `toolUpdate` entries. Every older version reads: a log before version 4
/// has no snapshots, and one before version 5 no commands or exit codes.
const LOG_VERSION: u32 = 5;

/// One run and its log.
pub(crate) struct Run {
    file: PathBuf,
    /// Where a summary record is appended once this run finishes, when configured.
    usage: Option<LedgerSink>,
    state: Mutex<RunState>,
}

/// Token and cost totals collected in memory while a run is live.
#[derive(Debug, Default)]
struct RunUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cached_read_tokens: Option<u64>,
    cached_write_tokens: Option<u64>,
    thought_tokens: Option<u64>,
    total_tokens: Option<u64>,
    context_used: Option<u64>,
    context_size: Option<u64>,
    /// Cumulative session cost before this turn began (for a kept agent reused on a follow-up).
    cost_baseline: Option<(f64, String)>,
    /// Latest cumulative session cost reported by the agent during this turn.
    cost_amount: Option<f64>,
    cost_currency: Option<String>,
}

impl RunUsage {
    fn record_update(&mut self, used: Option<u64>, size: Option<u64>, cost: Option<UsageCost>) {
        if let Some(used) = used {
            self.context_used = Some(used);
        }
        if let Some(size) = size {
            self.context_size = Some(size);
        }
        if let Some(cost) = cost
            && cost.amount.is_finite()
            && cost.amount >= 0.0
            && !cost.currency.is_empty()
        {
            self.cost_amount = Some(cost.amount);
            self.cost_currency = Some(cost.currency);
        }
    }

    /// Cost attributable to this turn alone: the delta from `cost_baseline` when this run
    /// continued an earlier turn on the same kept session with the same currency.
    fn turn_cost(&self) -> (Option<f64>, Option<String>) {
        let (Some(raw), Some(currency)) = (self.cost_amount, self.cost_currency.as_ref()) else {
            return (None, None);
        };
        let amount = match &self.cost_baseline {
            Some((base, base_currency)) if base_currency == currency && raw >= *base => {
                (raw - *base).max(0.0)
            }
            _ => raw,
        };
        (Some(amount), Some(currency.clone()))
    }

    /// Latest known cumulative session cost for a follow-up that reuses this session.
    fn session_cost(&self) -> Option<(f64, String)> {
        if let (Some(amount), Some(currency)) = (self.cost_amount, self.cost_currency.as_ref()) {
            return Some((amount, currency.clone()));
        }
        self.cost_baseline.clone()
    }

    fn record_prompt(&mut self, usage: PromptUsage) {
        if let Some(val) = usage.input_tokens {
            self.input_tokens = Some(val);
        }
        if let Some(val) = usage.output_tokens {
            self.output_tokens = Some(val);
        }
        if let Some(val) = usage.cached_read_tokens {
            self.cached_read_tokens = Some(val);
        }
        if let Some(val) = usage.cached_write_tokens {
            self.cached_write_tokens = Some(val);
        }
        if let Some(val) = usage.thought_tokens {
            self.thought_tokens = Some(val);
        }
        if let Some(val) = usage.total_tokens {
            self.total_tokens = Some(val);
        }
    }
}

pub(super) struct RunState {
    pub(super) header: Header,
    entries: Vec<Entry>,
    /// The log, open for appending while live; `None` after it ended or a write failed.
    log: Option<File>,
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
}

/// The first line of a run log.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Header {
    version: u32,
    /// `R1`, `R2`: the log's file name.
    pub(super) id: String,
    /// The agent's name in the settings.
    pub(super) agent: String,
    /// The ids of the notes sent.
    pub(super) notes: Vec<String>,
    /// The prompt's text, as the agent received it.
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
}

/// What a new run is: a send of notes, a reviewer's stage, or a follow-up. One value, so a
/// header never holds both a review and a follow-up.
#[derive(Debug)]
pub(super) enum Origin {
    Notes,
    Review(Stage),
    FollowUp(FollowUp),
}

/// A follow-up run's link to its thread: `of` is the run it follows, `text` what the engineer
/// wrote. The prompt holds more when a fresh agent had to be told the thread first.
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
    /// Milliseconds until a second Stop ends the agent, 0 once it would, while this process
    /// is stopping the run; the page offers Force stop from then on, after a reload too.
    force_in: Option<u64>,
    /// For a reviewer's run, which run it reviews and its place in the chain.
    review: Option<Stage>,
    /// For a follow-up, the run it follows and the engineer's words.
    follow_up: Option<FollowUp>,
}

impl Run {
    /// Creates the log for a new run in `dir`, with the next free id, takes the lock of the
    /// run's thread, `thread`, and writes the header. `runs/active.lock` is held only for
    /// this: it makes taking a number, counting the live runs and taking a thread's lock one
    /// step across windows, so no two windows both see a place under [`MAX_LIVE`].
    ///
    /// # Errors
    ///
    /// [`Error::RunElsewhere`] when another window's run works in the thread, or another
    /// window holds a lock this takes past [`LOCK_PATIENCE`]; [`Error::TooManyRuns`] when
    /// [`MAX_LIVE`] runs are live in the repository; [`Error::BadRequest`] for a thread id
    /// that is not a run's; [`Error::ReadRun`] when the thread locks cannot be listed or
    /// opened; and [`Error::WriteRun`] when a file cannot be written.
    pub(super) fn create(
        dir: &Path,
        opening: Opening<'_>,
        thread: InThread<'_>,
        usage: Option<LedgerSink>,
    ) -> Result<Self> {
        let Opening {
            agent,
            notes,
            prompt,
            origin,
        } = opening;
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
        // would show a new R5 as the old one.
        let used = last_run(&mut active).map_err(error(&active_path))?;
        let mut number = highest_run(dir).map_err(error(dir))?.max(used) + 1;
        // Creating the file claims the id; another window may have claimed it first.
        let (file, mut log) = loop {
            let file = dir.join(format!("R{number}.jsonl"));
            match File::options().append(true).create_new(true).open(&file) {
                Ok(log) => break (file, log),
                Err(source) if source.kind() == io::ErrorKind::AlreadyExists => number += 1,
                Err(source) => return Err(Error::WriteRun { path: file, source }),
            }
        };
        let (review, follow_up) = match origin {
            Origin::Notes => (None, None),
            Origin::Review(stage) => (Some(stage), None),
            Origin::FollowUp(follow_up) => (None, Some(follow_up)),
        };
        let header = Header {
            version: LOG_VERSION,
            id: format!("R{number}"),
            agent: agent.to_owned(),
            notes: notes.to_vec(),
            prompt: prompt.to_owned(),
            started_at: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |since| since.as_secs()),
            review,
            follow_up,
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

    /// The newest readable run log in `dir`, read as a run this process does not run; `None`
    /// when there is none. A newer log that cannot be read is skipped, and the run shown says
    /// why.
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
            match Self::read(&file) {
                Ok(run) => {
                    if let Some(skipped) = skipped {
                        run.state().problem.get_or_insert(skipped);
                    }
                    return Ok(Some(run));
                }
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
        let bytes = fs::read(file).map_err(|error| format!("it cannot be read ({error})"))?;
        let mut lines = bytes.split(|&byte| byte == b'\n');
        let header = parse_header(lines.next().unwrap_or_default())?;

        let mut state = RunState::new(header, None, None);
        for (index, line) in lines.enumerate() {
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
        Ok(Self {
            file: file.to_owned(),
            usage: None,
            state: Mutex::new(state),
        })
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
    pub(super) fn set_cost_baseline(&self, baseline: Option<(f64, String)>) {
        self.state().usage.cost_baseline = baseline;
    }

    /// The latest cumulative session cost reported on this run (or its baseline if none).
    pub(super) fn session_cost(&self) -> Option<(f64, String)> {
        self.state().usage.session_cost()
    }

    /// Adds an entry to the run and its log. A run that ended takes no more entries.
    pub(super) fn record(&self, entry: Entry) {
        let finished = {
            let mut state = self.state();
            if !state.live() {
                return;
            }
            let status = end_status(&entry);
            self.write(&mut state, entry);
            status.and_then(|status| self.usage_record(&state, status))
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

    /// Appends `entry` to the log and to `state`, which the caller holds.
    fn write(&self, state: &mut RunState, entry: Entry) {
        if let Some(log) = &mut state.log
            && let Err(error) = log.write_all(&json_line(&entry))
        {
            state.log = None;
            state.problem.get_or_insert(format!(
                "the run log {} could not be written, so this run will not be shown whole after \
                 a restart: {error}",
                self.file.display()
            ));
        }
        state.apply(&entry);
        state.entries.push(entry);
    }

    /// Records an agent's event, and returns the notes to mark sent now: the first sign that
    /// the agent took the turn (an update, a permission request, or a stop other than a
    /// refusal) releases them. A turn that fails or is refused at once leaves them open.
    /// `answer`, the event's auto-answer, is recorded with it, at once.
    pub(super) fn record_event(
        &self,
        event: Event,
        answer: Option<Entry>,
        root: &Path,
    ) -> Option<Vec<String>> {
        // One hold of the state for the event, its answer and the notes: a reader sees both
        // entries or neither, and a run that ended meanwhile neither takes the event nor marks
        // its notes sent. Taken first, since an update is read by the kind of its call.
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
                Event::TurnEnded(TurnEnd::Stopped { reason, usage }) => {
                    if let Some(usage) = usage {
                        state.usage.record_prompt(usage);
                    }
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
            let status = end_status(&entry);
            for entry in std::iter::once(entry).chain(answer) {
                self.write(&mut state, entry);
            }
            let finished = status.and_then(|status| self.usage_record(&state, status));
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

    fn usage_record(&self, state: &RunState, status: &'static str) -> Option<LedgerRecord> {
        let sink = self.usage.as_ref()?;
        let origin = if state.header.review.is_some() {
            "review"
        } else if state.header.follow_up.is_some() {
            "follow_up"
        } else {
            "notes"
        };
        let (cost_amount, cost_currency) = state.usage.turn_cost();
        Some(LedgerRecord {
            version: RECORD_VERSION,
            run_id: state.header.id.clone(),
            repo: sink.repo.clone(),
            agent: state.header.agent.clone(),
            model: state.model.clone(),
            origin: origin.to_owned(),
            status: status.to_owned(),
            started_at: state.header.started_at,
            ended_at: now_secs().max(state.header.started_at),
            input_tokens: state.usage.input_tokens,
            output_tokens: state.usage.output_tokens,
            cached_read_tokens: state.usage.cached_read_tokens,
            cached_write_tokens: state.usage.cached_write_tokens,
            thought_tokens: state.usage.thought_tokens,
            total_tokens: state.usage.total_tokens,
            context_used: state.usage.context_used,
            context_size: state.usage.context_size,
            cost_amount,
            cost_currency,
            tool_calls: state.kinds.len() as u64,
        })
    }

    fn write_usage(&self, record: Option<LedgerRecord>) {
        if let (Some(sink), Some(record)) = (&self.usage, record) {
            // Best effort: a read-only config dir never fails or stalls a run.
            drop(ledger::append(&sink.file, &record));
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
            force_in: state.cancel_asked.filter(|_| state.live()).map(|at| {
                u64::try_from(FORCE_AFTER.saturating_sub(at.elapsed()).as_millis())
                    .expect("at most FORCE_AFTER, 3 s")
            }),
            review: state.header.review.clone(),
            follow_up: state.header.follow_up.clone(),
        }
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
            log,
            lock,
            started: false,
            cancel_requested: false,
            cancel_asked: None,
            after_claimed: false,
            unmarked: None,
            model: None,
            usage: RunUsage::default(),
            end: None,
            requested: BTreeSet::new(),
            answered: BTreeSet::new(),
            kinds: HashMap::new(),
            problem: None,
        }
    }

    /// The agent's reply: its message parts joined, a blank line between messages.
    pub(super) fn reply(&self) -> String {
        let mut reply = String::new();
        let mut last: Option<&Option<String>> = None;
        for entry in &self.entries {
            if let Entry::Message { text, message } = entry {
                if last.is_some_and(|last| last != message) {
                    reply.push_str("\n\n");
                }
                reply.push_str(text);
                last = Some(message);
            }
        }
        reply
    }

    /// The run's work as a reviewer reads it: its prompt, its reply, and the diffs of its
    /// finished tool calls, each call's last report, in the order the calls were made.
    pub(super) fn work(&self) -> Work {
        let mut calls: Vec<(&str, &[Output], bool)> = Vec::new();
        for entry in &self.entries {
            match entry {
                Entry::Tool {
                    id, status, output, ..
                } => {
                    let done = *status == ToolCallStatus::Completed;
                    match calls.iter_mut().find(|call| call.0 == id) {
                        Some(call) => *call = (id, output, done),
                        None => calls.push((id, output, done)),
                    }
                }
                Entry::ToolUpdate {
                    id, status, output, ..
                } => {
                    if let Some(call) = calls.iter_mut().find(|call| call.0 == id) {
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
        Work {
            prompt: self.header.prompt.clone(),
            reply: self.reply(),
            diffs,
        }
    }

    /// Whether this process runs the run and it has not ended.
    pub(super) fn live(&self) -> bool {
        self.lock.is_some()
    }

    /// Keeps the fields that sum up the entries current with one more.
    fn apply(&mut self, entry: &Entry) {
        match entry {
            Entry::Started => self.started = true,
            Entry::CancelRequested => self.cancel_requested = true,
            Entry::Permission { request, .. } | Entry::Question { request, .. } => {
                self.requested.insert(*request);
            }
            Entry::Answer { request, .. } | Entry::Reply { request, .. } => {
                self.answered.insert(*request);
            }
            Entry::Ended { .. } | Entry::Failed { .. } => {
                self.end = Some(if matches!(entry, Entry::Ended { .. }) {
                    Status::Ended
                } else {
                    Status::Failed
                });
                self.log = None;
                // Another window may start a run in the thread now, or end it.
                self.lock = None;
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
            | Entry::Snapshot { .. } => {}
        }
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
    take_lock(&file).map_err(|source| match source {
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
    let number = id.strip_prefix('R')?.parse::<u64>().ok()?;
    (format!("R{number}") == id).then_some(number)
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
    // `Origin` makes the two exclusive when Postil writes a header; this holds a hand edit to it.
    if header.review.is_some() && header.follow_up.is_some() {
        return Err("its header is both a review and a follow-up".to_owned());
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
        // An image or a link in a message, usage updates (tracked on RunState), and unknown updates.
        SessionUpdate::AgentMessageChunk { .. }
        | SessionUpdate::AgentThoughtChunk { .. }
        | SessionUpdate::UsageUpdate { .. }
        | SessionUpdate::Other => return None,
    };
    Some(entry)
}

/// The ledger status string when `entry` ends a run; `None` for any other entry.
fn end_status(entry: &Entry) -> Option<&'static str> {
    match entry {
        Entry::Ended {
            reason: StopReason::Cancelled,
        } => Some("cancelled"),
        Entry::Ended { .. } => Some("ended"),
        Entry::Failed { .. } => Some("failed"),
        _ => None,
    }
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

    /// A run of note N1 by agent "Fake", in a new thread, in the runs directory `dir`.
    fn create(dir: &Path, prompt: &str) -> Run {
        let opening = Opening {
            agent: "Fake",
            notes: &["N1".to_owned()],
            prompt,
            origin: Origin::Notes,
        };
        Run::create(dir, opening, InThread::New, None).unwrap()
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
        run.state().log = Some(File::open(&run.file).unwrap());

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
        ] {
            run.record(entry);
        }

        let work = run.state().work();
        let afters: Vec<_> = work.diffs.iter().map(|diff| diff.after.as_str()).collect();
        assert_eq!(afters, ["first", "second"], "the update reached t1 only");
        assert_eq!(work.reply, "one part\n\ntwo");
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
}
