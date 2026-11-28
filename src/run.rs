//! Runs: notes sent to the agent as one prompt, and everything the agent reports back.
//!
//! A [`Runner`] belongs to the open project. Each send starts a thread, which works in a
//! worktree and on a branch of its own (`thread.rs`, docs/design.md "Worktrees"). The runner
//! holds a [`Slot`] per thread, and keeps each thread's agent alive between its runs, with its
//! session, so a follow-up reaches the same conversation; the owner chose this over a fresh
//! agent per run on 2026-11-23. An agent is replaced when a run names another agent or it
//! exited, let go when the window needs its place ([`Slots::make_room`]), and stopped when its
//! thread ends, the project closes or the app quits.
//!
//! Threads run side by side, one run at a time each: a live run holds an exclusive lock on its
//! thread's `threads/<id>.lock`, so another window cannot start one there either, and at most
//! [`MAX_LIVE`] runs are live in a repository, counted across windows. Ending or forgetting a
//! thread takes its lock, so no window's agent works in a worktree being freed.
//!
//! The run log, a run's record in memory and on disk, is in [`log`].
//!
//! # Stopping
//!
//! Stop asks the agent to cancel the turn (`session/cancel`). An agent that is still
//! starting is killed instead, and the run ends at once: a start can take minutes. Asked
//! again from [`FORCE_AFTER`] on, Stop kills an agent that did not stop, so a hung agent never
//! holds the working tree; a second Stop sooner is the same request.
//!
//! # Threads and locks
//!
//! Requests from the page run on one request thread (`shell.rs`), the agent's events on its
//! reader thread, and an agent's start on a thread of its own. A review's next stage starts
//! on the reader thread of the stage that ended ([`Runner::advance`]). Locks are always
//! taken in this order: the runner's [`Slots`], the repository's snapshot lock (inside
//! [`Repo::snapshot`], which takes no other), a run's state, then the agent connection's
//! own locks (inside [`Agent`]'s methods, which take none of the others). The record's
//! `threads.lock` (inside `thread.rs`, which takes no other) may be taken while the slots
//! are held, so nothing that holds it may take them. A launch takes `runs/active.lock` and a
//! thread's lock under the slots ([`Run::create`]), and only tries each for a moment; ending
//! or forgetting a thread takes the thread's lock first and the slots after, and does so only
//! while the thread is not busy in this window, when no launch there can come until that
//! request is answered: only a request starts a run in an idle thread. Eviction ([`room`]),
//! on another run's start thread, takes an idle thread's lock first and the slots after too:
//! a launch there meanwhile cannot take that lock, and is refused as during an ending, and a
//! launch holding the slots only tries the lock for a moment, so neither waits on the other
//! for long. No snapshot is taken
//! under a run's state. The slots are held around the agent's calls that answer, cancel and
//! prompt (`auto_answer`, [`Runner::cancel`], `begin`), and around git in a thread's
//! worktree: the commit before a run (`add`, `diff --cached`, `commit`, `rev-parse`, tens of
//! milliseconds on a medium repository, as a snapshot is), and a read-only run's check and
//! undo as its turn ends (`status`, `branch`, `rev-parse`, `checkout -B`, `clean`), each after
//! the few `rev-parse` calls that open the worktree ([`thread::open`]). Every page request, and
//! every other thread's agent events, wait for those: one lock for all threads keeps the lock
//! order above, and costs milliseconds.
//!
//! `review.lock` (inside `notes.rs`) and `analysis.lock` (inside `analysis.rs`) are the
//! innermost locks: whatever holds one reads and writes files only, and takes no slots,
//! `runs/active.lock` or thread lock. An analysis run's merge takes `analysis.lock` on its
//! agent's reader thread as its turn ends ([`finish_analysis`]), with no slots held and while
//! the run still holds its thread's lock, for the read, the merge and the save of the file:
//! milliseconds, since the reply's citations are checked before it is taken. An ask saves its note
//! under it inside [`Run::create`], so under the slots and `runs/active.lock`
//! ([`Runner::ask`]), and so does an analysis run that moves `checked`: the forced update's
//! claim ([`Runner::analyze_checked`]) and an Update of every diagram ([`Runner::analyze`]).
//! The save holds it for milliseconds; a lock another process keeps is waited for up to
//! `files::LOCK_WAIT`, 2 s, before the ask fails. Every page request and agent event waits
//! that long then, and another window's send gives up on `runs/active.lock` after its 200 ms
//! (`RunElsewhere`). Accepted over trying the lock for
//! that same moment: only a script holding `review.lock` costs it, and a second way of
//! taking the lock would serve this one caller.
//! The page is told about every change through `notify`, and fetches what it has not seen.

mod log;
pub(crate) mod pipeline;
mod pull;
mod room;

#[cfg(test)]
pub(crate) use room::{FLOOR, TestDisk};

use std::collections::{HashMap, HashSet};
use std::fmt::{self, Write as _};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::acp::{
    Agent, ContentBlock, ElicitationReply, Event, Launch, McpServer, Opened, PermissionKind,
    PermissionOption, StopReason, Stopper, ToolKind, TurnEnd,
};
use crate::agents::{self, AgentChoice, Scanner};
use crate::analysis::{self, AnalysisJob, CommitId, Plan, Planned};
use crate::error::{Error, Result, error_chain};
use crate::memory::{Memory, RunMemory};
use crate::notes::{self, NewNote, Prompt, Review};
use crate::repo::Repo;
use crate::report;
use crate::review::{self, Diff, Finding, Stage, Work};
use crate::run::log::{
    Changes, Entry, FollowUp, Header, InThread, MAX_LIVE, Opening, Origin, Point, Remember, Run,
    RunView, held_threads, hold_thread_lock, read_header, remove_thread_lock, run_number,
    run_numbers,
};
use crate::settings::TeamMemory;
use crate::thread::{self, Mode, State, Thread};
use crate::usage::ledger::LedgerSink;

/// How long an agent may take to start, and then to open a session. Claude's adapter took
/// 4.3 s to open a session with npx's cache warm (2026-11-23); a first `npx` run downloads
/// the adapter, which takes tens of seconds on a slow link. Past this, the run fails with
/// the hint to run the agent in a terminal (an agent waiting for a browser sign-in never
/// answers). Stop ends the wait sooner.
const START_TIMEOUT: Duration = Duration::from_secs(120);

/// How long after the first Stop a second one kills the agent. Arbitrary: longer than a double
/// click or a second Escape, short enough that a hung agent is not waited on. The page learns
/// when from [`RunView`]'s `force_in`.
const FORCE_AFTER: Duration = Duration::from_secs(3);

/// Tells the page that a run changed. In the app it wakes the event loop, which calls the
/// page; tests count the calls.
pub(crate) type Notify = Arc<dyn Fn() + Send + Sync>;

/// Maximum number of disk-read historical runs cached in memory per [`Runner`].
const DISK_RUN_CACHE_CAP: usize = 8;

/// Maximum number of changed file paths included in a review's memory recall queries.
const MAX_REVIEW_QUERY_PATHS: usize = 16;

/// A run log read from disk, cached by `(id, modified, len)` so repeated views skip re-parsing.
struct CachedDiskRun {
    id: String,
    modified: Option<std::time::SystemTime>,
    len: u64,
    run: Arc<Run>,
}

/// The runs of the open project, and the agent kept between them.
pub(crate) struct Runner {
    repo: Arc<Repo>,
    agents: Scanner,
    /// Team memory, which a fresh agent gets when the run asks for it ([`Memory::for_run`]).
    memory: Memory,
    notify: Notify,
    /// Postil's home folder, `~/.postil`, under which threads' worktrees live
    /// (`thread.rs`); `None` when the home folder is unknown, which refuses every send.
    home: Option<PathBuf>,
    /// Durable global usage ledger (`<config dir>/usage.jsonl`), when configured.
    usage_file: Option<PathBuf>,
    slots: Mutex<Slots>,
    /// Bounded LRU cache of disk-read runs (`Run::read`), validated by `(modified, len)`.
    disk_runs: Mutex<Vec<CachedDiskRun>>,
    /// What this window is doing about each thread's pull request, by thread id
    /// ([`pull::PullWork`]); a lock never held with another.
    pull_work: Mutex<HashMap<String, pull::PullWork>>,
    /// Persisted Lane pipelines (`Easy`, `Middle`, `Hard`), keyed by root thread id.
    pipelines: Mutex<HashMap<String, pipeline::PipelineRun>>,
    /// Holds a review's next stage, once, until a test lets it start
    /// ([`Runner::hold_next_stage`]): between a stage's end and the next stage's start is a
    /// window no request can otherwise be timed into.
    #[cfg(test)]
    next_stage_gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    /// Holds the next worktree's making, once, until a test lets it go
    /// ([`Runner::hold_making`]): a checkout takes milliseconds in a test, too short to Stop
    /// or end a thread during it otherwise.
    #[cfg(test)]
    making_gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    /// Holds the next analysis merge, once, once it claimed the run's end, until a test lets it
    /// go ([`Runner::hold_merge`]): a merge takes milliseconds, too short to Stop or close the
    /// project during it otherwise. The sender says the merge got there.
    #[cfg(test)]
    merge_gate: Mutex<Option<MergeGate>>,
    /// The disk bounds a test sets in place of the real ones ([`room::TestDisk`]).
    #[cfg(test)]
    test_disk: Mutex<room::TestDisk>,
}

/// What a runner holds now: a slot per thread this window ran in.
#[derive(Debug, Default)]
struct Slots {
    /// By thread id, its first run's.
    by_thread: HashMap<String, Slot>,
    /// This window's latest run, which `GET api/run` follows.
    latest: Option<Arc<Run>>,
    /// Counts launches, to tell which kept agent was used least recently ([`Slot::used`]).
    launches: u64,
    /// Set when the project closed: an agent that finishes starting afterwards is stopped.
    closed: bool,
}

/// One thread's runs in this window, and the agent kept between them.
#[derive(Debug)]
struct Slot {
    agent: Held,
    /// Counts the agents this slot let go. An agent's events and its start carry the number
    /// it was started with: when it differs, the agent was replaced, stopped, or its run
    /// cancelled, and what it reports is dropped.
    generation: u64,
    /// The thread's latest run in this window.
    run: Option<Arc<Run>>,
    /// The thread, as read at its latest launch: the agent's working directory is its
    /// worktree.
    thread: Thread,
    /// Whether the start thread is making the thread's worktree (`git worktree add`, the
    /// clones), from the launch until it is made or fails. Git writes the worktree meanwhile,
    /// so a commit, a run or an ending there could act on a half-made checkout: the slot counts
    /// as busy, and the run stays live, holding the thread's lock, past a Stop or a close,
    /// until the start thread ends it. Making cannot be cut short.
    making: bool,
    /// [`Slots::launches`] at the slot's latest launch.
    used: u64,
    /// The review under way, while its stages run one after another; cleared when a stage
    /// does not end well or reports edits it may not make, when the engineer stops it, by a
    /// send, when the project closes, and when the last stage ends; `advance` takes it to
    /// start the next stage. From a stage's end until `advance` starts the next one, `stage`
    /// is the stage to start, not the slot's run's: a late event from the ended agent no
    /// longer ends the review.
    chain: Option<Chain>,
}

/// A review under way (`review.rs`): the stage now running, the reviewers, what they review,
/// and the findings proposed so far. Its `Debug` counts the findings: their text quotes code.
struct Chain {
    stage: Stage,
    reviewers: Vec<AgentChoice>,
    work: Work,
    /// The reviewed run's notes: a review run carries them, so it goes with them when the
    /// last is deleted.
    notes: Vec<String>,
    /// The files the reviewed run changed, linked in each reviewer's prompt.
    files: Vec<String>,
    findings: Vec<Finding>,
    /// Whether each stage's agent gets team memory, as the settings said when the review began.
    memory: TeamMemory,
}

impl fmt::Debug for Chain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Chain")
            .field("stage", &self.stage)
            .field("reviewers", &self.reviewers)
            .field("work", &self.work)
            .field("findings", &self.findings.len())
            .finish_non_exhaustive()
    }
}

/// The runner's agent: none, one starting, or one ready and kept between runs.
#[derive(Debug, Default)]
enum Held {
    #[default]
    None,
    /// Its process runs and its session is being opened, on the start thread.
    Starting(Stopper),
    /// Its session is open; it is kept for the next run while it is the one chosen.
    Ready {
        choice: AgentChoice,
        agent: Agent,
        /// Whether its session got hippius-mem's server: a memory draft never goes to it then
        /// (`Runner::draft`).
        memory: TeamMemory,
    },
}

impl Slots {
    /// Whether the agent started in thread `thread` as number `generation` was let go
    /// meanwhile: replaced, stopped, its thread ended, or the project closed.
    fn let_go(&self, thread: &str, generation: u64) -> bool {
        self.closed
            || self
                .by_thread
                .get(thread)
                .is_none_or(|slot| slot.generation != generation)
    }

    /// The slot whose latest run is `run` (`R2`), if that run is live.
    fn live_slot(&mut self, run: &str) -> Option<&mut Slot> {
        self.by_thread.values_mut().find(|slot| {
            slot.run.as_ref().is_some_and(|own| {
                let state = own.state();
                state.live() && state.header.id == run
            })
        })
    }

    /// The first of the notes `ids` that a live run of this window carries, with that run's
    /// id; `None` when no live run carries any of them.
    fn carrying(&self, ids: &[String]) -> Option<(String, String)> {
        self.by_thread
            .values()
            .filter_map(|slot| slot.run.as_ref())
            .find_map(|run| {
                let state = run.state();
                let note = ids.iter().find(|id| state.header.notes.contains(id))?;
                state
                    .live()
                    .then(|| (note.clone(), state.header.id.clone()))
            })
    }

    /// The live run of notes of this window started most recently, if any: the latest run once
    /// the one before is gone. An analysis run and a memory draft are never the latest: the Run
    /// tab lists runs of notes.
    fn newest_live(&self) -> Option<Arc<Run>> {
        self.by_thread
            .values()
            .filter_map(|slot| slot.run.as_ref())
            .filter(|run| {
                let state = run.state();
                state.live() && state.header.analysis.is_none() && state.header.remember.is_none()
            })
            .max_by_key(|run| run_number(&run.state().header.id))
            .cloned()
    }

    /// Whether any thread of this window is busy ([`Slot::busy`]).
    fn busy(&self) -> bool {
        self.by_thread.values().any(Slot::busy)
    }

    /// Whether an analysis run of this window is under way, or its worktree is being made.
    fn analyzing(&self) -> bool {
        self.by_thread.values().any(|slot| {
            slot.run.as_ref().is_some_and(|run| {
                let state = run.state();
                state.header.analysis.is_some() && (state.live() || slot.making)
            })
        })
    }

    /// Whether `thread` holds a live ready agent matching `choice`.
    fn has_live_agent(&self, thread: &str, choice: &AgentChoice) -> bool {
        self.by_thread.get(thread).is_some_and(|slot| {
            let Held::Ready {
                choice: kept,
                agent: live,
                ..
            } = &slot.agent
            else {
                return false;
            };
            kept == choice && live.is_alive()
        })
    }

    /// Before a fresh agent starts in `thread`, lets go of the agent of the idle slot used
    /// least recently when the other threads hold `MAX_LIVE - 1` agents or more, so this
    /// window holds at most [`MAX_LIVE`]: an idle agent is a process of a few hundred MB kept
    /// only for its thread's next follow-up, which a fresh agent can take with the
    /// conversation so far. Runs in other windows hold no agent here, so only this window's
    /// are counted. A busy slot keeps its agent: a review between two stages is busy with no
    /// live run, so the window can hold one agent more until a later launch.
    #[must_use = "drop the released agent after the slots' lock: dropping it waits for its process"]
    fn make_room(&mut self, thread: &str) -> Held {
        let others = self
            .by_thread
            .iter()
            .filter(|(id, slot)| id.as_str() != thread && !matches!(slot.agent, Held::None))
            .count();
        if others < MAX_LIVE - 1 {
            return Held::None;
        }
        self.by_thread
            .iter_mut()
            .filter(|(id, slot)| {
                id.as_str() != thread && !slot.busy() && !matches!(slot.agent, Held::None)
            })
            .min_by_key(|(_, slot)| slot.used)
            .map_or(Held::None, |(_, slot)| slot.release())
    }
}

impl Slot {
    fn new(thread: Thread) -> Self {
        Self {
            agent: Held::None,
            generation: 0,
            run: None,
            thread,
            making: false,
            used: 0,
            chain: None,
        }
    }

    /// Lets the agent go: a starting one is stopped here, under the caller's lock (that waits
    /// for `/bin/kill`, milliseconds), and anything either would still report is dropped. The
    /// caller drops the returned agent after releasing the slots' lock, which stops a ready
    /// one.
    #[must_use = "drop the released agent after the slots' lock: dropping it waits for its process"]
    fn release(&mut self) -> Held {
        if let Held::Starting(stopper) = &self.agent {
            stopper.stop();
        }
        self.generation += 1;
        std::mem::take(&mut self.agent)
    }

    /// [`Runner::busy`]: a run under way, a review between two stages, or a worktree being
    /// made.
    fn busy(&self) -> bool {
        if self.making {
            return true;
        }
        let Some(run) = &self.run else {
            return false;
        };
        let state = run.state();
        // Between two stages `decide` has moved the chain on to the next one, so it names
        // another stage than the run that ended. A stage whose agent failed to start ended
        // with the chain still on it: no stage follows that one.
        state.live()
            || self
                .chain
                .as_ref()
                .is_some_and(|chain| state.header.review.as_ref() != Some(&chain.stage))
    }
}

/// How [`Runner::analyze_checked`] answered.
#[derive(Debug)]
pub(crate) enum StartedUpdate {
    /// Run `run` started, and `checked` moved to the commit it starts from, which people know
    /// by `commit`, its short id.
    Started { run: String, commit: String },
    /// Another window moved `checked` first, and started the update there.
    AlreadyChecked,
    /// The run could not start, for the reason given; `checked` stays, so the next check tries
    /// again.
    Blocked(String),
}

/// The refusal of an analysis run while one of this window is under way.
fn analysis_under_way() -> Error {
    Error::AnalysisRefused(
        "an analysis of this window is under way: wait for it, or stop it".to_owned(),
    )
}

/// A send that waits for its agent to start.
#[derive(Debug)]
struct Pending {
    agent: AgentChoice,
    run: Arc<Run>,
    ids: Vec<String>,
    prompt: Prompt,
    /// Whether the agent gets team memory. The prompt was built with the line about it when
    /// hippius-mem was Ready as the run was sent ([`Runner::prompt_memory`]); the start gives
    /// the line to an agent that got the server, and only to it.
    memory: TeamMemory,
    generation: u64,
    /// The thread the agent works in.
    thread: Thread,
    worktree: Worktree,
    /// For a review's first stage, the reviewed run's snapshots: its start recalls the team
    /// memories for the review ([`Runner::recall_for_review`]).
    review_recall: Option<ReviewSnapshots>,
}

/// The snapshots taken before and after a reviewed run, when it has them: the paths that
/// differ between them are the ones it changed, a command's edits included.
#[derive(Debug)]
struct ReviewSnapshots {
    before: Option<String>,
    after: Option<String>,
}

/// Whether a run's worktree is still to be made, on the start thread, before its agent.
#[derive(Debug)]
enum Worktree {
    /// A new thread's, on a new branch: a worktree that cannot be made takes the thread out
    /// of the record. The run records the fork commit as its snapshot before when `snapshot`
    /// (its thread works).
    Make {
        snapshot: bool,
    },
    /// An evicted thread's, again from its branch. The run records the commit it starts from
    /// as its snapshot before when `snapshot` (it may change files).
    Remake {
        snapshot: bool,
    },
    Made,
}

/// A run for [`Runner::launch`] to start. What it carries and sends, its [`Content`], is
/// passed beside it: an ask's exists only once the run has its place.
#[derive(Debug)]
struct NewRun {
    agent: AgentChoice,
    origin: Origin,
    conversation: Conversation,
    /// Whether a fresh agent gets team memory; a kept one has the session it had.
    memory: TeamMemory,
    /// The review the run is a stage of, which becomes the one under way; `None` ends the
    /// review under way, if any.
    review: Option<Chain>,
    /// As [`Pending::review_recall`].
    review_recall: Option<ReviewSnapshots>,
    /// Forces this individual stage to run read-only even inside a `Mode::Work` thread (e.g.,
    /// Hard Lane Planner / Plan Judge / Reviewers).
    stage_read_only: bool,
    place: Place,
}

/// The notes a launched run carries, in its header (it goes with the last of them), and the
/// prompt it sends.
#[derive(Debug)]
struct Content {
    notes: Vec<String>,
    prompt: Prompt,
}

/// A run whose log [`Runner::create_run`] made: the prompt it sends, and the notes it marks
/// sent once its agent takes the turn.
#[derive(Debug)]
struct Created {
    run: Arc<Run>,
    prompt: Prompt,
    marks: Vec<String>,
}

/// Which thread a launched run works in.
#[derive(Debug)]
enum Place {
    /// A new thread in `mode`, started at commit `fork` and named from `title`, its first
    /// note's text.
    New {
        fork: String,
        title: String,
        mode: Mode,
    },
    /// The open thread with this record's id, whose worktree is on disk: read again by
    /// `launch` once it holds the thread's lock, as ending a thread does, so another window
    /// cannot end or forget it in between. Only its mode, which never changes, is taken from
    /// this record.
    Open(Thread),
}

impl Place {
    /// The mode of the thread the run works in.
    fn mode(&self) -> Mode {
        match self {
            Self::New { mode, .. } => *mode,
            Self::Open(thread) => thread.mode,
        }
    }

    /// The thread whose lock the run takes ([`Run::create`]).
    fn in_thread(&self) -> InThread<'_> {
        match self {
            Self::New { .. } => InThread::New,
            Self::Open(thread) => InThread::Open(&thread.id),
        }
    }
}

/// How the engineer ends a thread (docs/design.md, "Ending a thread").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ending {
    /// Writes the thread's changes into the engineer's working tree.
    Apply,
    /// Ends it with nothing applied.
    Discard,
}

/// Which agent a launched run goes to.
#[derive(Debug)]
enum Conversation {
    /// The kept agent, whose conversation the run continues, when it is the run's agent and
    /// alive; otherwise a fresh one. A follow-up checks the same, under the same lock, to
    /// choose its prompt.
    Kept,
    /// A fresh agent, which replaces the kept one.
    Fresh,
}

impl Runner {
    #[expect(
        clippy::too_many_arguments,
        reason = "each is a separate part of the window the runner shares, from one caller \
                  (`Site::open`); a struct to carry them would serve that call alone"
    )]
    pub(crate) fn new(
        repo: Arc<Repo>,
        agents: Scanner,
        memory: Memory,
        notify: Notify,
        home: Option<PathBuf>,
        usage_file: Option<PathBuf>,
    ) -> Arc<Self> {
        if let Some(home) = &home {
            // Finishes deleting the worktrees of threads that ended while the app last ran:
            // a quit can cut a background delete short.
            thread::empty_trash(&thread::trash_for(repo.root(), home));
            // Runs git in each folder no thread names, so off the request thread. An error
            // reading the record shows at the first send.
            let (repo, home) = (Arc::clone(&repo), home.clone());
            let swept = std::thread::Builder::new()
                .name("sweep-worktrees".to_owned())
                .spawn(move || drop(thread::sweep(&repo, &home)));
            // No thread: the folders stay until the next open, which is no harm.
            drop(swept);
        }
        let pipelines = pipeline::load_pipelines(&repo.pipelines_file());
        Arc::new(Self {
            repo,
            agents,
            memory,
            notify,
            home,
            usage_file,
            slots: Mutex::new(Slots::default()),
            disk_runs: Mutex::new(Vec::new()),
            pull_work: Mutex::default(),
            pipelines: Mutex::new(pipelines),
            #[cfg(test)]
            next_stage_gate: Mutex::new(None),
            #[cfg(test)]
            making_gate: Mutex::new(None),
            #[cfg(test)]
            merge_gate: Mutex::new(None),
            #[cfg(test)]
            test_disk: Mutex::default(),
        })
    }

    /// The prompt the notes `ids` make, as the agent will receive it: its text and the files
    /// linked after it, with the line about team memory when `memory` is on and Ready.
    ///
    /// # Errors
    ///
    /// As [`notes::prompt`].
    pub(crate) fn preview(&self, ids: &[String], memory: TeamMemory) -> Result<Prompt> {
        notes::prompt(&self.repo, ids, self.prompt_memory(memory))
    }

    /// Whether a prompt of notes has the line about team memory: when `memory` is on and
    /// hippius-mem is Ready now. Never waits for a check ([`Memory::is_ready`]).
    fn prompt_memory(&self, memory: TeamMemory) -> TeamMemory {
        if memory == TeamMemory::On && self.memory.is_ready() {
            TeamMemory::On
        } else {
            TeamMemory::Off
        }
    }

    /// Starts a run: sends the notes `ids` to a fresh `agent`, in a new thread (docs/design.md,
    /// "Worktrees"). The thread's record is written, and its branch starts at the commit
    /// `HEAD` names now; its worktree is made on the agent's start thread, before the agent
    /// starts there. Returns at once, except while another window holds `runs/active.lock`,
    /// when it waits up to `LOCK_PATIENCE` for it, and when the count of live runs is at the
    /// cap, when it waits up to `LOCK_PATIENCE` more for one to end, both under the runner's
    /// lock, before it refuses. The run reports its progress through its log.
    ///
    /// # Errors
    ///
    /// [`Error::BadRequest`] when a run of this window under way carries one of the notes; as
    /// [`Run::create`]: [`Error::TooManyRuns`] while the most runs a repository may have are
    /// live, and [`Error::RunElsewhere`] while another window holds `runs/active.lock`; as
    /// [`notes::prompt`]; [`Error::NoCommit`] in a repository with no commit; and the errors
    /// of [`thread::plan`] and [`thread::add`], which fail the run.
    pub(crate) fn send(
        self: &Arc<Self>,
        agent: AgentChoice,
        ids: &[String],
        memory: TeamMemory,
    ) -> Result<()> {
        // Fast pre-check before disk/git I/O, then drop `slots` so other threads do not stall.
        {
            let slots = self.slots();
            if let Some((note, run)) = slots.carrying(ids) {
                return Err(Error::BadRequest(format!(
                    "{note} is in {run}, which is under way: wait for it, or stop it"
                )));
            }
        }
        let prompt = notes::prompt(&self.repo, ids, self.prompt_memory(memory))?;
        let fork = self.repo.head()?.ok_or(Error::NoCommit)?;
        let title = notes::body(&self.repo, &ids[0])?;
        let slots = self.slots();
        // A note is marked sent only once its agent takes the turn, so until then it reads as
        // open: re-check under the lock so a concurrent send cannot put it in two threads at once.
        if let Some((note, run)) = slots.carrying(ids) {
            return Err(Error::BadRequest(format!(
                "{note} is in {run}, which is under way: wait for it, or stop it"
            )));
        }
        let new = NewRun {
            agent,
            memory,
            origin: Origin::Notes,
            // A new thread works in a new worktree, which a kept agent's session is not in.
            conversation: Conversation::Fresh,
            review: None,
            review_recall: None,
            stage_read_only: false,
            place: Place::New {
                fork,
                title,
                mode: Mode::Work,
            },
        };
        let content = Content {
            notes: ids.to_vec(),
            prompt,
        };
        self.launch(slots, new, |_| Ok(content))
    }

    /// Asks the question `new` of a fresh `agent`, read-only, in a new thread of its own that
    /// only reads (docs/review-assist.md, "Ask without changing"), and returns every note, the
    /// ask last. The ask is saved as a note, sent in the run made for it, only once that run has
    /// its place, under the lock that counts the live runs (`Run::create`), so a refused ask
    /// leaves nothing behind (decided 2026-11-26), and an ask whose run fails before its agent
    /// takes the turn shows that run's failure. Waits for locks as [`Runner::send`] does, and
    /// for `review.lock` under them (module docs, "Threads and locks").
    ///
    /// # Errors
    ///
    /// [`Error::NoCommit`] in a repository with no commit; [`Error::NoHome`]; as
    /// [`thread::all`] for a record that cannot be read; as [`notes::check_new`] for the
    /// question; and as [`Runner::send`]: [`Error::TooManyRuns`], [`Error::RunElsewhere`], and
    /// the errors that fail the run. Only those after the note is saved leave it, with its
    /// failed run.
    pub(crate) fn ask(
        self: &Arc<Self>,
        agent: AgentChoice,
        new: NewNote,
        memory: TeamMemory,
    ) -> Result<Review> {
        let fork = self.repo.head()?.ok_or(Error::NoCommit)?;
        // Checked before the note is saved, where `thread::plan` and `thread::add` would fail
        // the run only after it.
        self.home.as_deref().ok_or(Error::NoHome)?;
        thread::all(&self.repo)?;
        let checked = notes::check_new(&self.repo, new)?;
        let title = checked.body().to_owned();
        let new = NewRun {
            agent,
            memory,
            origin: Origin::Notes,
            conversation: Conversation::Fresh,
            review: None,
            review_recall: None,
            stage_read_only: false,
            place: Place::New {
                fork,
                title,
                mode: Mode::Read,
            },
        };
        let mut saved = None;
        self.launch(self.slots(), new, |run| {
            let ask = notes::save_ask(&self.repo, checked, run)?;
            saved = Some(ask.review);
            Ok(Content {
                notes: vec![ask.id],
                prompt: ask.prompt,
            })
        })?;
        Ok(saved.expect("launch succeeded, so Run::create called the content closure"))
    }

    /// Whether an analysis run of this window is under way, or its worktree is being made.
    /// Takes the slots' lock for the look only.
    pub(crate) fn analyzing(&self) -> bool {
        self.slots().analyzing()
    }

    /// Starts an analysis run of `agent`, read-only in a new thread of its own that ends with
    /// the run (docs/review-assist.md, "An analysis run"), as `job` asks, and returns the run's
    /// id; or, for an Update of every diagram with nothing to send while the checkout moved
    /// to another history ([`Plan::MoveChecked`]), moves `checked` there and returns `None`. Its
    /// reply is merged into the analysis as its turn ends well ([`finish_analysis`]). An Update
    /// of every diagram moves `checked` to the run's fork as it starts, under the slots and
    /// `runs/active.lock` ([`analysis::move_checked`]). Waits for the agent scan, once per app,
    /// and for locks as [`Runner::send`] does. Reads the analysis, and for an update the file
    /// list and the record ([`analysis::plan`]).
    ///
    /// # Errors
    ///
    /// [`Error::AnalysisRefused`] when `agent` is not installed, or an analysis of this window
    /// is under way, both before a thread is made; [`Error::NoCommit`]; [`Error::NoHome`]; as
    /// [`thread::all`] and [`analysis::plan`]; and as [`Runner::send`]:
    /// [`Error::TooManyRuns`], [`Error::RunElsewhere`], and the errors that fail the run.
    pub(crate) fn analyze(
        self: &Arc<Self>,
        agent: AgentChoice,
        job: AnalysisJob,
        memory: TeamMemory,
    ) -> Result<Option<String>> {
        self.analysis_ready(&agent)?;
        let fork = CommitId::of_head(&self.repo)?.ok_or(Error::NoCommit)?;
        self.home.as_deref().ok_or(Error::NoHome)?;
        thread::all(&self.repo)?;
        let (planned, moves_checked) = match analysis::plan(&self.repo, job)? {
            Plan::Run {
                planned,
                moves_checked,
            } => (planned, moves_checked),
            Plan::MoveChecked(head) => {
                analysis::move_checked(&self.repo, &head)?;
                return Ok(None);
            }
        };
        let mut id = None;
        self.start_analysis(agent, memory, &fork, planned, |run| {
            if moves_checked {
                analysis::move_checked(&self.repo, &fork)?;
            }
            id = Some(run.to_owned());
            Ok(())
        })?;
        Ok(Some(id.expect(
            "start_analysis succeeded, so it called the opening closure",
        )))
    }

    /// Starts the forced update `planned` (docs/review-assist-plan.md, 8.7) of `agent`, as
    /// [`Runner::analyze`] starts an analysis, when it can claim `checked` from `expected` to
    /// `head` ([`analysis::claim_check`]): under the slots and `runs/active.lock`, once the cap
    /// of 3 live runs is checked and before the run's log exists, so a refused start leaves
    /// `checked` as it was, and of two windows only the first starts a run.
    ///
    /// # Errors
    ///
    /// Those of [`Runner::analyze`] other than the reasons it reports in
    /// [`StartedUpdate::Blocked`]: an Analyzer that is not installed, an analysis of this
    /// window under way, no home folder, a worktree path with a colon, the cap of 3, another
    /// window's lock.
    #[expect(
        clippy::too_many_arguments,
        reason = "the Analyzer and its team memory, then the claim and the run it starts: \
                  each a value the caller has apart, none of them a group elsewhere"
    )]
    pub(crate) fn analyze_checked(
        self: &Arc<Self>,
        agent: AgentChoice,
        memory: TeamMemory,
        expected: Option<&CommitId>,
        head: &CommitId,
        planned: Planned,
    ) -> Result<StartedUpdate> {
        let blocked = |error: &Error| Ok(StartedUpdate::Blocked(error_chain(error)));
        // Before the claim, which they would otherwise fail only after: the thread's place
        // is planned once the run has its number (`Runner::place`).
        let ready = self
            .analysis_ready(&agent)
            .and_then(|()| thread::all(&self.repo))
            .and_then(|_| {
                let home = self.home.as_deref();
                thread::plan(&self.repo, home, "R1", head.as_str(), &planned.title)
            });
        match ready {
            Ok(_) => {}
            Err(
                error @ (Error::AnalysisRefused(_) | Error::NoHome | Error::ColonInPath { .. }),
            ) => return blocked(&error),
            Err(error) => return Err(error),
        }
        let mut lost = false;
        let mut id = None;
        let started = self.start_analysis(agent, memory, head, planned, |run| {
            if !analysis::claim_check(&self.repo, expected, head)? {
                lost = true;
                return Err(Error::AnalysisRefused(
                    "another window started this update".to_owned(),
                ));
            }
            id = Some(run.to_owned());
            Ok(())
        });
        match started {
            Ok(()) => Ok(StartedUpdate::Started {
                run: id.expect("start_analysis succeeded, so it called the opening closure"),
                commit: head.short().to_owned(),
            }),
            Err(_) if lost => Ok(StartedUpdate::AlreadyChecked),
            Err(error @ Error::AnalysisRefused(_)) => blocked(&error),
            // Another window's start causes these too: when it claimed `checked`, the update
            // is under way there.
            Err(error @ (Error::TooManyRuns { .. } | Error::RunElsewhere)) => {
                if analysis::is_checked_at(&self.repo, head)? {
                    Ok(StartedUpdate::AlreadyChecked)
                } else {
                    blocked(&error)
                }
            }
            Err(error) => Err(error),
        }
    }

    /// Refuses an analysis run of `agent` before its plan: an Analyzer that is not installed
    /// would fail only on the start thread, after the thread is made; an analysis of this window
    /// under way has not written the analysis the plan reads (an update would be refused as
    /// having nothing to update), and the plan runs git for an update. Checked again under the
    /// slots' lock ([`Runner::start_analysis`]), which a run may have taken since.
    ///
    /// # Errors
    ///
    /// [`Error::AnalysisRefused`] saying which.
    fn analysis_ready(&self, agent: &AgentChoice) -> Result<()> {
        if !agents::is_installed(agent, &self.agents.wait()) {
            return Err(Error::AnalysisRefused(format!(
                "{} is not installed: install it, or choose another Analyzer in Settings",
                agent.name
            )));
        }
        if self.analyzing() {
            return Err(analysis_under_way());
        }
        Ok(())
    }

    /// Launches the analysis run `planned` of `agent`, with team memory as `memory` says, in a
    /// new read-only thread at commit `fork`, calling `opening` with the run's id under the
    /// slots and `runs/active.lock` before its log exists ([`Run::create`]): an error there
    /// starts nothing.
    ///
    /// # Errors
    ///
    /// [`Error::AnalysisRefused`] when an analysis of this window is under way; the error of
    /// `opening`; and as [`Runner::launch`].
    #[expect(
        clippy::too_many_arguments,
        reason = "the two callers differ in the fork and the opening only: a struct of the \
                  rest would be built once at each call and taken apart at once"
    )]
    fn start_analysis(
        self: &Arc<Self>,
        agent: AgentChoice,
        memory: TeamMemory,
        fork: &CommitId,
        planned: Planned,
        opening: impl FnOnce(&str) -> Result<()>,
    ) -> Result<()> {
        let Planned { run, title, prompt } = planned;
        let slots = self.slots();
        if slots.analyzing() {
            return Err(analysis_under_way());
        }
        let new = NewRun {
            agent,
            memory,
            origin: Origin::Analysis(run),
            conversation: Conversation::Fresh,
            review: None,
            review_recall: None,
            stage_read_only: false,
            place: Place::New {
                fork: fork.as_str().to_owned(),
                title,
                mode: Mode::Read,
            },
        };
        self.launch(slots, new, |run| {
            opening(run)?;
            Ok(Content {
                notes: Vec::new(),
                prompt: Prompt {
                    text: prompt,
                    files: Vec::new(),
                },
            })
        })
    }

    /// The thread `id`, open with its worktree on disk, or evicted, whose run makes it again.
    ///
    /// # Errors
    ///
    /// [`Error::BadRequest`] when it has no thread (it ran before threads had worktrees), the
    /// thread ended, or the worktree of an open one is gone; [`Error::NotAWorktree`] when the
    /// record puts it where Postil puts no worktree; [`Error::NoHome`]; as [`thread::find`].
    fn open_thread(&self, id: &str) -> Result<Thread> {
        let thread = thread::find(&self.repo, id)?.ok_or_else(|| {
            Error::BadRequest(format!(
                "{id} ran before each thread had a worktree of its own: send its notes again"
            ))
        })?;
        let ended = match thread.state {
            // Made again where Postil puts worktrees: checked before it is made.
            State::Evicted => {
                let home = self.home.as_deref().ok_or(Error::NoHome)?;
                thread::check_place(&self.repo, home, &thread)?;
                return Ok(thread);
            }
            State::Open if thread.path.is_dir() => {
                // The record is a file a hand edit can point anywhere: a commit, an agent and
                // a proposer's undo go only into a worktree of Postil's ([`thread::open`]).
                let home = self.home.as_deref().ok_or(Error::NoHome)?;
                thread::open(&self.repo, home, &thread)?;
                return Ok(thread);
            }
            State::Open => {
                let again = match thread.mode {
                    Mode::Work => "send its notes again",
                    Mode::Read => "ask the question again",
                };
                return Err(Error::BadRequest(format!(
                    "the worktree of {id}'s thread is gone ({}): discard the thread, and \
                     {again}",
                    thread.path.display()
                )));
            }
            State::Applied => "applied",
            State::Discarded => "discarded",
        };
        // An ask's thread ends when its note is resolved: a new question is a new ask.
        let again = match thread.mode {
            Mode::Work => "send its notes again to work on them",
            Mode::Read => "ask again to go on",
        };
        Err(Error::BadRequest(format!(
            "{id}'s thread was {ended}: {again}"
        )))
    }

    /// Sends `text`, the engineer's follow-up to run `of`, to `agent` as a new run that
    /// follows it (docs/design.md, "Follow-ups"). It goes to the same conversation when the
    /// kept agent is `agent` and ran `of`, this window's latest run. Otherwise a fresh agent
    /// gets the thread's earlier prompts and replies before the text. The run carries `of`'s
    /// notes, so it goes with them, and marks none sent. A thread does not fork: `of` must be
    /// its latest run. Reads the log of every run in the thread and the header of every newer
    /// run, and takes a snapshot and may wait for a lock as [`Runner::send`] does.
    ///
    /// # Errors
    ///
    /// [`Error::TurnRunning`] while a run of this thread is under way in this window, and
    /// [`Error::RunElsewhere`] while another window's is; [`Error::BadRequest`] for a blank
    /// text, a run that is a review, an analysis or a memory draft, or a run another run
    /// already follows; as
    /// [`Runner::view_of`] for `of` and the runs it follows, and [`Error::NoSuchRun`] when one
    /// follows a run that is not older; [`Error::ReadRun`] when the runs directory cannot be
    /// listed; [`Error::WriteRun`] when the run log cannot be created; and as
    /// [`Runner::open_thread`] for the thread, and [`Runner::launch`].
    pub(crate) fn follow_up(
        self: &Arc<Self>,
        of: &str,
        agent: AgentChoice,
        text: &str,
        memory: TeamMemory,
    ) -> Result<()> {
        let text = text.trim();
        if text.is_empty() {
            return Err(Error::BadRequest("the follow-up has no text".to_owned()));
        }
        let thread = self.thread(of)?;
        let followed = thread
            .last()
            .expect("a thread holds at least the run it ends with");
        let notes = {
            let state = followed.state();
            if state.header.review.is_some() {
                return Err(Error::BadRequest(format!(
                    "{of} is a review: follow up the run it reviews"
                )));
            }
            // Its thread ended with it, and a follow-up would merge nothing.
            if state.header.analysis.is_some() {
                return Err(Error::BadRequest(format!(
                    "{of} is an analysis: analyze again to go on"
                )));
            }
            // A draft is no turn of its thread: a follow-up of it would start a thread whose
            // history is the draft alone.
            if let Some(remember) = &state.header.remember {
                return Err(Error::BadRequest(format!(
                    "{of} is a memory draft: follow up {}",
                    remember.of
                )));
            }
            state.header.notes.clone()
        };
        // Two follow-ups of one run would make two threads that each leave the other out.
        if let Some(next) = self.followed_by(of)? {
            return Err(Error::BadRequest(format!(
                "{next} already follows {of}: follow up {next}"
            )));
        }
        let first = thread[0].state().header.id.clone();
        // Refused here, before a run log is made, when the thread is not open; `launch` reads
        // it again under the thread's lock.
        let record = self.open_thread(&first)?;
        let evicted = record.state == State::Evicted;
        let slots = self.slots();
        let kept = kept_agent(&slots, &first, evicted, followed).map(|kept| kept.choice);
        let same_conversation = kept.is_some();
        let agent = kept.unwrap_or(agent);
        let prompt = match (same_conversation, record.mode) {
            // After the words, so the page tells this prompt from a fresh agent's by its start
            // (`ui/status.js`, `freshAgent`).
            (true, Mode::Work) => Prompt {
                text: format!("{text}\n\n{}", report::ASK_AGAIN),
                files: Vec::new(),
            },
            // A thread that only reads asks for no report, and its agent was told the rules.
            (true, Mode::Read) => Prompt {
                text: text.to_owned(),
                files: Vec::new(),
            },
            (false, mode) => Prompt {
                text: fresh_follow_up(&thread, text, mode),
                files: thread_files(&thread, mode),
            },
        };
        let new = NewRun {
            agent,
            memory,
            origin: Origin::FollowUp(FollowUp {
                of: of.to_owned(),
                text: text.to_owned(),
            }),
            conversation: if same_conversation {
                Conversation::Kept
            } else {
                Conversation::Fresh
            },
            review: None,
            review_recall: None,
            stage_read_only: false,
            place: Place::Open(record),
        };
        self.launch(slots, new, |_| Ok(Content { notes, prompt }))
    }

    /// The latest turn of the thread that starts with run `first`, the run a note was sent in:
    /// `first` or its latest follow-up, where a memory is drafted ([`Runner::draft`]). `None`
    /// when the run has no thread (it ran before threads had worktrees), or its thread ended
    /// (applied or discarded): the Remember dialog then starts from the note's text. Reads the
    /// record, and the header of every newer run for each follow-up.
    ///
    /// # Errors
    ///
    /// [`Error::BadRequest`] when `first` is not a run id; as [`thread::find`];
    /// [`Error::ReadRun`] when the runs directory cannot be listed.
    pub(crate) fn thread_tip(&self, first: &str) -> Result<Option<String>> {
        let first_number = run_number(first).ok_or_else(|| {
            Error::BadRequest(format!("run id {first:?}: expected R and a number"))
        })?;
        let Some(record) = thread::find(&self.repo, first)? else {
            return Ok(None);
        };
        if matches!(record.state, State::Applied | State::Discarded) {
            return Ok(None);
        }
        let dir = self.repo.runs_dir();
        let mut numbers = run_numbers(&dir).map_err(|source| Error::ReadRun {
            path: dir.clone(),
            source,
        })?;
        numbers.retain(|&n| n > first_number);
        numbers.sort_unstable();
        let mut tip = first.to_owned();
        // Follow-ups always have higher run numbers than the run they follow, so one ascending
        // pass over newer headers finds the tip in O(M) instead of O(K * M).
        for newer in numbers {
            if let Ok(header) = read_header(&dir.join(format!("R{newer}.jsonl")))
                && header
                    .follow_up
                    .as_ref()
                    .is_some_and(|follow_up| follow_up.of == tip)
            {
                tip = header.id;
            }
        }
        Ok(Some(tip))
    }

    /// Asks the thread whose latest turn is run `of` to draft a team memory about `note`, as a
    /// note's prompt quotes it, and returns the draft run's id (docs/review-assist.md, "Postil
    /// writes memory"). The run is read-only whatever its thread's mode, marked `remember: {of}`
    /// in its header, and its reply ends with the draft ([`RunView`]'s `draft`). It goes to the
    /// thread's kept agent when that agent ran `of`, as a follow-up does, and otherwise to a
    /// fresh `agent` told the thread so far. It carries `of`'s notes, so it goes with them, and
    /// is never the Run tab's latest run. Waits for locks as [`Runner::follow_up`] does.
    ///
    /// A fresh agent gets no team memory: a draft run must not write memory itself, and the
    /// recall it would offer is the engineer's next step, in the dialog. A kept agent whose
    /// session got hippius-mem's server is never used: a fresh agent takes its place.
    ///
    /// # Errors
    ///
    /// [`Error::BadRequest`] for a run that is a review, an analysis or a memory draft, or one
    /// another run already follows; and as [`Runner::follow_up`] for the thread and its launch.
    pub(crate) fn draft(
        self: &Arc<Self>,
        of: &str,
        agent: AgentChoice,
        note: &str,
    ) -> Result<String> {
        let thread = self.thread(of)?;
        let followed = thread
            .last()
            .expect("a thread holds at least the run it ends with");
        let notes = {
            let state = followed.state();
            let kind = if state.header.review.is_some() {
                Some("a review")
            } else if state.header.analysis.is_some() {
                Some("an analysis")
            } else if state.header.remember.is_some() {
                Some("a memory draft")
            } else {
                None
            };
            if let Some(kind) = kind {
                return Err(Error::BadRequest(format!(
                    "{of} is {kind}: draft from the thread of a note"
                )));
            }
            state.header.notes.clone()
        };
        if let Some(next) = self.followed_by(of)? {
            return Err(Error::BadRequest(format!(
                "{next} already follows {of}: draft after {next}"
            )));
        }
        let first = thread[0].state().header.id.clone();
        let record = self.open_thread(&first)?;
        let evicted = record.state == State::Evicted;
        let slots = self.slots();
        // Never an agent whose session has hippius-mem's server (decided 2026-11-27): a memory
        // it wrote would be an external write the engineer did not save, and its session cannot
        // be taken back. A fresh agent then starts, told the thread so far, with none; the kept
        // agent is let go, and its conversation with it.
        let kept = kept_agent(&slots, &first, evicted, followed)
            .filter(|kept| kept.memory == TeamMemory::Off)
            .map(|kept| kept.choice);
        let prompt = match &kept {
            Some(_) => Prompt {
                text: draft_ask(note),
                files: Vec::new(),
            },
            None => Prompt {
                text: fresh_draft(&thread, note, record.mode),
                files: thread_files(&thread, record.mode),
            },
        };
        let new = NewRun {
            conversation: if kept.is_some() {
                Conversation::Kept
            } else {
                Conversation::Fresh
            },
            agent: kept.unwrap_or(agent),
            memory: TeamMemory::Off,
            origin: Origin::Remember(Remember { of: of.to_owned() }),
            review: None,
            review_recall: None,
            stage_read_only: false,
            place: Place::Open(record),
        };
        let mut id = None;
        self.launch(slots, new, |run| {
            id = Some(run.to_owned());
            Ok(Content { notes, prompt })
        })?;
        Ok(id.expect("launch succeeded, so Run::create called the content closure"))
    }

    /// The runs of the thread that ends with run `of`: the run of notes it started from, then
    /// each follow-up in order, `of` last.
    fn thread(&self, of: &str) -> Result<Vec<Arc<Run>>> {
        let mut thread = vec![self.run_by_id(of)?];
        // The number of the log last read: its file's, not its header's, which a hand edit
        // could change.
        let mut number = run_number(of).expect("run_by_id read run `of`, so its id parses");
        loop {
            let before = thread
                .last()
                .expect("the thread starts with one run")
                .state()
                .header
                .follow_up
                .as_ref()
                .map(|follow_up| follow_up.of.clone());
            let Some(before) = before else { break };
            // A follow-up is created after the run it follows, so each step goes to a lower
            // number and the walk ends. A log edited by hand could say otherwise.
            number = run_number(&before)
                .filter(|older| *older < number)
                .ok_or_else(|| Error::NoSuchRun {
                    id: format!("R{number}"),
                    problem: format!("it follows {before}, which is not an older run"),
                })?;
            thread.push(self.run_by_id(&before)?);
        }
        thread.reverse();
        Ok(thread)
    }

    /// The run that already follows run `of`, if any, read from the headers of the newer
    /// logs. A log that cannot be read is passed over: it follows nothing the page can show.
    fn followed_by(&self, of: &str) -> Result<Option<String>> {
        let number = run_number(of).expect("follow_up read run `of`, so its id parses");
        let dir = self.repo.runs_dir();
        let numbers = run_numbers(&dir).map_err(|source| Error::ReadRun {
            path: dir.clone(),
            source,
        })?;
        Ok(numbers
            .into_iter()
            .filter(|newer| *newer > number)
            .find_map(|newer| {
                let header = read_header(&dir.join(format!("R{newer}.jsonl"))).ok()?;
                (header.follow_up?.of == of).then_some(header.id)
            }))
    }

    /// Starts a review of run `of` by `reviewers`, in order (`review.rs`): each stage is a run
    /// of its own, started with a fresh agent when the one before it ends well. The judge's
    /// stage takes snapshots as [`Runner::send`] does; a proposer, which may not edit, none.
    ///
    /// # Errors
    ///
    /// [`Error::TurnRunning`] while a run of the thread is under way in this window, and
    /// [`Error::RunElsewhere`] while another window's is; [`Error::BadRequest`] with no
    /// reviewers, for a run that is itself a review, a follow-up, or a run that only read; as
    /// [`Runner::view_of`] for the reviewed run; [`Error::WriteRun`] when the first stage's
    /// log cannot be created; and as [`Runner::open_thread`] for the thread, and
    /// [`Runner::launch`].
    pub(crate) fn review(
        self: &Arc<Self>,
        of: &str,
        reviewers: Vec<AgentChoice>,
        memory: TeamMemory,
    ) -> Result<()> {
        if reviewers.is_empty() {
            return Err(Error::BadRequest(
                "no reviewers are set: add them in Settings".to_owned(),
            ));
        }
        let reviewed = self.run_by_id(of)?;
        let (notes, work, files, snapshots) = {
            let state = reviewed.state();
            if state.header.review.is_some() {
                return Err(Error::BadRequest(format!("{of} is itself a review")));
            }
            if state.header.read_only {
                return Err(Error::BadRequest(format!(
                    "{of} only read: there are no changes to review"
                )));
            }
            // A review reviews the run of notes a thread started from (docs/design.md,
            // "Follow-ups"); the page shows it there.
            if let Some(follow_up) = &state.header.follow_up {
                return Err(Error::BadRequest(format!(
                    "{of} is a follow-up of {}: review the run its thread started from",
                    follow_up.of
                )));
            }
            let work = state.work();
            let files = edited_files(&work.diffs);
            let snapshots = ReviewSnapshots {
                before: state.snapshot(Point::Before).map(str::to_owned),
                after: state.snapshot(Point::After).map(str::to_owned),
            };
            (state.header.notes.clone(), work, files, snapshots)
        };
        let chain = Chain {
            stage: Stage {
                of: of.to_owned(),
                reviewer: 1,
                reviewers: reviewers.len(),
            },
            reviewers,
            work,
            notes,
            files,
            findings: Vec::new(),
            memory,
        };
        self.start_stage(self.slots(), chain, Some(snapshots))
    }

    /// Reads only the header of run `of` (line 1 of its log), avoiding full `.jsonl` entry
    /// parsing when callers only need metadata such as `agent`, `notes`, or `follow_up`.
    fn header_by_id(&self, of: &str) -> Result<log::Header> {
        let number = run_number(of)
            .ok_or_else(|| Error::BadRequest(format!("run id {of:?}: expected R and a number")))?;
        {
            let slots = self.slots();
            if let Some(run) = slots
                .by_thread
                .values()
                .filter_map(|slot| slot.run.as_ref())
                .chain(slots.latest.as_ref())
                .find(|run| run.state().header.id == of)
            {
                return Ok(run.state().header.clone());
            }
        }
        let no_such = |problem: String| Error::NoSuchRun {
            id: of.to_owned(),
            problem,
        };
        let file = self.repo.runs_dir().join(format!("R{number}.jsonl"));
        if !file.exists() {
            return Err(no_such("there is no such run log".to_owned()));
        }
        read_header(&file).map_err(no_such)
    }

    /// The run `of`, this window's when it is one of its threads' latest, else read from its
    /// log.
    fn run_by_id(&self, of: &str) -> Result<Arc<Run>> {
        let number = run_number(of)
            .ok_or_else(|| Error::BadRequest(format!("run id {of:?}: expected R and a number")))?;
        let own = {
            let slots = self.slots();
            slots
                .by_thread
                .values()
                .filter_map(|slot| slot.run.as_ref())
                .chain(slots.latest.as_ref())
                .find(|run| run.state().header.id == of)
                .cloned()
        };
        if let Some(run) = own {
            return Ok(run);
        }
        let no_such = |problem: String| Error::NoSuchRun {
            id: of.to_owned(),
            problem,
        };
        let file = self.repo.runs_dir().join(format!("R{number}.jsonl"));
        let Ok(meta) = std::fs::metadata(&file) else {
            return Err(no_such("there is no such run log".to_owned()));
        };
        let modified = meta.modified().ok();
        let len = meta.len();
        {
            let mut cache = self
                .disk_runs
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(pos) = cache
                .iter()
                .position(|entry| entry.id == of && entry.len == len && entry.modified == modified)
            {
                let entry = cache.remove(pos);
                let run = Arc::clone(&entry.run);
                cache.push(entry);
                return Ok(run);
            }
        }
        let run = Run::read(&file).map(Arc::new).map_err(no_such)?;
        {
            let mut cache = self
                .disk_runs
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(pos) = cache.iter().position(|entry| entry.id == of) {
                cache.remove(pos);
            } else if cache.len() >= DISK_RUN_CACHE_CAP {
                cache.remove(0);
            }
            cache.push(CachedDiskRun {
                id: of.to_owned(),
                modified,
                len,
                run: Arc::clone(&run),
            });
        }
        Ok(run)
    }

    /// Starts `chain`'s current stage as a new run, with a fresh agent, so a reviewer never
    /// inherits the reviewed agent's conversation; `chain` becomes the review under way. The
    /// stage works in the reviewed run's thread, read again for each stage. The first stage
    /// gets the reviewed run's `snapshots`, and its start recalls the review's team memories,
    /// which the chain then carries to the stages after it.
    fn start_stage(
        self: &Arc<Self>,
        slots: MutexGuard<'_, Slots>,
        chain: Chain,
        snapshots: Option<ReviewSnapshots>,
    ) -> Result<()> {
        // Refused here, before a run log is made; `launch` reads it again under the thread's
        // lock.
        let record = self.open_thread(&chain.stage.of)?;
        let text = if chain.stage.judges() {
            review::judge_prompt(&chain.work, &chain.findings)
        } else {
            review::propose_prompt(&chain.work)
        };
        let content = Content {
            notes: chain.notes.clone(),
            prompt: Prompt {
                text,
                files: chain.files.clone(),
            },
        };
        let new = NewRun {
            agent: chain.reviewers[chain.stage.reviewer - 1].clone(),
            memory: chain.memory,
            origin: Origin::Review(chain.stage.clone()),
            conversation: Conversation::Fresh,
            place: Place::Open(record),
            review: Some(chain),
            review_recall: snapshots,
            stage_read_only: false,
        };
        self.launch(slots, new, |_| Ok(content))
    }

    /// Starts the review's next stage, once `goes_on` moved the chain to it as stage `ended`
    /// ended. Runs on the ended agent's reader thread, and replaces that agent there, as
    /// [`on_event`] does with an agent that exited: [`Agent`]'s drop kills and reaps the
    /// process but never joins its threads, and the reader holds none of the agent's locks
    /// while it delivers an event.
    fn advance(self: &Arc<Self>, thread: &str, ended: &Arc<Run>) {
        let mut slots = self.slots();
        // A stop, the thread's end, or another review meanwhile took the chain, or replaced
        // its run.
        let Some(slot) = slots.by_thread.get_mut(thread) else {
            return;
        };
        if !slot.run.as_ref().is_some_and(|run| Arc::ptr_eq(run, ended)) {
            return;
        }
        let Some(chain) = slot.chain.take() else {
            return;
        };
        if let Err(error) = self.start_stage(slots, chain, None) {
            // The chain went with the stage that could not start, so the review ends, and the
            // stage that ended says why.
            ended.state().problem.get_or_insert(format!(
                "the next reviewer could not start: {}",
                error_chain(&error)
            ));
            (self.notify)();
        }
    }

    /// Makes `new` its thread's run and its review the one under way there, and this window's
    /// latest run, sends the run to its agent, then tells the page. What the run carries and
    /// sends comes from `content`, called once the run has its place ([`Run::create`]). The
    /// thread is placed as [`Runner::place`] says. A new thread's snapshot before, the commit
    /// it starts from, is recorded on the start thread once its worktree is made
    /// ([`Runner::start`]). The slot changes only once the run's log exists, so a launch that
    /// fails leaves the review and the kept agent as they were. The agents it replaces or lets
    /// go are dropped after `slots` is released.
    ///
    /// A run in a thread that only reads ([`Mode::Read`]) is read-only in its header, so every
    /// run of the thread is, follow-ups included, whatever started them; it takes no snapshot.
    /// A proposing reviewer takes none either, and a memory draft, read-only in any thread,
    /// neither: it commits what was left in the worktree before it, as every run does, and its
    /// edits are undone.
    ///
    /// # Errors
    ///
    /// [`Error::TurnRunning`] while a run of the thread is under way in this window, as
    /// [`Run::create`]; the error of `content`; and as [`Runner::place`], which fails the run
    /// too.
    fn launch(
        self: &Arc<Self>,
        mut slots: MutexGuard<'_, Slots>,
        new: NewRun,
        content: impl FnOnce(&str) -> Result<Content>,
    ) -> Result<()> {
        // A worktree still being made counts as a run under way: its agent has not started.
        // Between two review stages the chain has been taken by `advance`, which launches the
        // next one, so the thread is not busy for it.
        if let Place::Open(thread) = &new.place
            && slots.by_thread.get(&thread.id).is_some_and(Slot::busy)
        {
            return Err(Error::TurnRunning);
        }
        let NewRun {
            agent,
            memory,
            origin,
            conversation,
            review,
            review_recall,
            stage_read_only,
            place,
        } = new;
        let snapshot = place.mode() == Mode::Work
            && !stage_read_only
            && !matches!(&origin, Origin::Review(stage) if !stage.judges())
            && !matches!(origin, Origin::Remember(_));
        let listed = !matches!(origin, Origin::Analysis(_) | Origin::Remember(_));
        let Created { run, prompt, marks } =
            self.create_run(&agent.name, origin, &place, stage_read_only, content)?;
        let (thread, worktree) = match self.place(&run, place, snapshot) {
            Ok(placed) => placed,
            Err(error) => {
                run.record(Entry::Failed {
                    error: error_chain(&error),
                });
                (self.notify)();
                return Err(error);
            }
        };

        let id = thread.id.clone();
        // Only in a worktree on disk: a follow-up chose the kept agent before it held the
        // thread's lock, and another window may have evicted the thread since. Its worktree is
        // then made again on the start thread, which a reused agent would skip.
        let reuse = matches!(worktree, Worktree::Made)
            && matches!(conversation, Conversation::Kept)
            && slots.has_live_agent(&id, &agent);
        // A fresh agent may take the place of another thread's idle one.
        let evicted = if reuse {
            Held::None
        } else {
            slots.make_room(&id)
        };
        slots.launches += 1;
        let used = slots.launches;
        // The Run tab follows the latest run, and lists runs of notes only.
        if listed {
            slots.latest = Some(Arc::clone(&run));
        }
        let slot = slots
            .by_thread
            .entry(id.clone())
            .or_insert_with(|| Slot::new(thread.clone()));
        slot.chain = review;
        let new_thread = matches!(worktree, Worktree::Make { .. });
        slot.making = !matches!(worktree, Worktree::Made);
        let prev_session_cost = reuse.then(|| slot.run.as_deref()?.session_cost()).flatten();
        slot.run = Some(Arc::clone(&run));
        slot.thread = thread.clone();
        slot.used = used;
        let replaced = if reuse && let Held::Ready { agent: kept, .. } = &slot.agent {
            run.set_cost_baseline(prev_session_cost);
            begin(kept, &run, &marks, &prompt, &thread.path);
            Held::None
        } else {
            // The thread's agent kept from its last run, if any, is stopped when this is
            // dropped.
            let replaced = slot.release();
            let pending = Pending {
                agent,
                memory,
                run: Arc::clone(&run),
                ids: marks,
                prompt,
                generation: slot.generation,
                thread,
                worktree,
                review_recall,
            };
            if let Err(error) = self.start(pending) {
                // No thread will make the worktree: the thread goes, as one whose worktree
                // could not be made does, before the failure ends the run and with it the
                // thread's lock, so no window takes the thread meanwhile.
                slot.making = false;
                if new_thread {
                    self.drop_new_thread(&id);
                }
                run.record(Entry::Failed {
                    error: format!("cannot start a thread for the agent: {error}"),
                });
            }
            replaced
        };
        drop(slots);
        // Dropping an agent waits for its process to be reaped.
        drop(replaced);
        drop(evicted);
        (self.notify)();
        Ok(())
    }

    /// Creates the log of a run of `agent` in `place`, started as `origin` says
    /// ([`Run::create`]), with the notes and prompt `content` gives once the run has its place.
    /// Its header is read-only when its thread only reads, and for a memory draft.
    ///
    /// # Errors
    ///
    /// As [`Run::create`], and the error of `content`.
    #[expect(
        clippy::too_many_arguments,
        reason = "each parameter is an independent launch property passed once from `Runner::launch`"
    )]
    fn create_run(
        &self,
        agent: &str,
        origin: Origin,
        place: &Place,
        stage_read_only: bool,
        content: impl FnOnce(&str) -> Result<Content>,
    ) -> Result<Created> {
        let read_only =
            stage_read_only || place.mode() == Mode::Read || matches!(origin, Origin::Remember(_));
        // A follow-up and a review mark no note sent: the notes went to the run they follow or
        // review. An ask, the run of notes in a thread that only reads, is saved sent in its
        // run already (`notes::save_ask`).
        let marks_sent = matches!(origin, Origin::Notes) && !read_only;
        let mut sent = None;
        let opening = |id: &str| {
            let Content { notes, prompt } = content(id)?;
            let text = prompt.text.clone();
            sent = Some(prompt);
            Ok(Opening {
                agent,
                notes,
                prompt: text,
                origin,
                read_only,
            })
        };
        let dir = self.repo.runs_dir();
        let run = Run::create(&dir, place.in_thread(), self.usage_sink(), opening)?;
        let marks = if marks_sent {
            run.state().header.notes.clone()
        } else {
            Vec::new()
        };
        Ok(Created {
            run: Arc::new(run),
            prompt: sent.expect("Run::create succeeded, so it called the opening closure"),
            marks,
        })
    }

    /// The thread `run` works in, as `place` says, and whether its worktree is still to be
    /// made. Called while `run` holds the thread's lock, so the thread is read under it. In an
    /// open thread, first commits what was left in the worktree and takes that commit as the
    /// snapshot before the run, when `snapshot`.
    ///
    /// # Errors
    ///
    /// For a new thread as [`thread::plan`] and [`thread::add`], the thread's lock file then
    /// removed; for an open one as [`Runner::open_thread`]; and without `snapshot` (a
    /// proposer, or a thread that only reads), as [`Runner::commit_worktree`].
    fn place(&self, run: &Run, place: Place, snapshot: bool) -> Result<(Thread, Worktree)> {
        let (mut thread, worktree) = match place {
            Place::New { fork, title, mode } => match self.new_thread(run, &fork, &title, mode) {
                Ok(thread) => (thread, Worktree::Make { snapshot }),
                Err(error) => {
                    // No record names the thread, so nothing would ever forget its lock file.
                    // Removed while the run still holds the lock (its failure is recorded
                    // after this returns), so no window can take the thread, and lock the file
                    // being removed, meanwhile.
                    let id = run.state().header.id.clone();
                    remove_thread_lock(&self.repo.runs_dir(), &id);
                    return Err(error);
                }
            },
            Place::Open(record) => {
                let id = record.id;
                let thread = self.open_thread(&id)?;
                let worktree = match thread.state {
                    State::Evicted => Worktree::Remake { snapshot },
                    State::Open | State::Applied | State::Discarded => Worktree::Made,
                };
                // Eviction takes the thread whose latest run is oldest first. A record that
                // cannot be written leaves it older than it is, which is no harm.
                drop(thread::update(&self.repo, &id, |stored| {
                    stored.last_run = thread::now();
                }));
                (thread, worktree)
            }
        };
        match worktree {
            // Its snapshot before is the fork commit, or the branch's commit for an evicted
            // thread, recorded on the start thread once the worktree is made: a run whose
            // worktree could not be made has no changes view, which would compare the
            // engineer's own files with that commit.
            Worktree::Make { .. } | Worktree::Remake { .. } => {}
            // Every run of an open thread starts from a commit of what was left there, a
            // read-only one's too: undoing its edits goes back to that commit, which must
            // hold the engineer's own commits and edits.
            Worktree::Made => match self.commit_worktree(run, &thread, Point::Before) {
                Ok(commit) => {
                    thread.tip.clone_from(&commit);
                    if snapshot {
                        run.record_snapshot(Point::Before, commit);
                    }
                }
                // A read-only run whose start could not be marked would be blamed, and undone,
                // for what was left: it does not start.
                Err(error) if !snapshot => return Err(error),
                Err(error) => no_snapshot(run, &error),
            },
        }
        Ok((thread, worktree))
    }

    /// Stops run `id` (module docs, "Stopping").
    ///
    /// # Errors
    ///
    /// [`Error::BadRequest`] when it is not under way in this window; [`Error::AgentExited`]
    /// when the agent is gone (its exit then ends the run).
    pub(crate) fn cancel(&self, id: &str) -> Result<()> {
        let mut slots = self.slots();
        let slot = slots
            .live_slot(id)
            .ok_or_else(|| Error::BadRequest(format!("{id} is not under way in this window")))?;
        let run = Arc::clone(
            slot.run
                .as_ref()
                .expect("live_slot found the slot by its run"),
        );
        let (started, asked) = {
            let state = run.state();
            (state.started, state.cancel_asked)
        };
        let thread = Some(slot.thread.clone());
        let mut released = Held::None;
        let mut forced = false;
        let mut result = Ok(());
        // A stopped reviewer ends its review: the next stage does not start.
        slot.chain = None;
        let cancel_thread_id = slot.thread.id.clone();
        match (started, asked) {
            (false, _) if slot.making => {
                // Git is still making the worktree, which cannot be cut short. The run, and
                // with it the thread's lock that keeps other windows out of the worktree,
                // stays until git is done: the start thread ends it then.
                released = slot.release();
                run.record(Entry::CancelRequested);
            }
            (false, _) => {
                // The agent is still starting: it is stopped, and the run ends now.
                released = slot.release();
                run.record(Entry::CancelRequested);
                run.record(Entry::Ended {
                    reason: StopReason::Cancelled,
                });
            }
            // A second Stop soon after the first is the same request (a double click, a
            // second Escape), not a force.
            (true, Some(at)) if at.elapsed() < FORCE_AFTER => {}
            (true, Some(_)) => {
                // The agent did not stop when asked, so it is ended, and its exit is not
                // reported over this. The run ends once the agent is gone (below).
                released = slot.release();
                forced = true;
            }
            (true, None) => {
                run.state().cancel_asked = Some(Instant::now());
                run.record(Entry::CancelRequested);
                if let Held::Ready { agent, .. } = &slot.agent {
                    // The requests this answered `cancelled` on the wire are closed in the log
                    // too, including one whose event is still on its way.
                    match agent.cancel() {
                        Ok(numbers) => {
                            for request in numbers {
                                run.record(Entry::Answer {
                                    request,
                                    option: None,
                                });
                            }
                        }
                        Err(error) => result = Err(error),
                    }
                }
            }
        }
        drop(slots);
        self.stop_pipeline(&cancel_thread_id);
        // Dropping the agent kills it and waits for its process to be reaped.
        drop(released);
        // An analysis merging its reply ends the run itself, once it is done.
        if forced && run.claim_end() {
            // Its exit is not reported, so the snapshot after it is taken here, once nothing
            // can write any more. The run stays live until then, so a send is refused
            // (`TurnRunning`) meanwhile.
            undo_if_live(self, &run, thread.as_ref());
            self.record_end_snapshot(&run, thread.as_ref());
            run.record(Entry::Failed {
                error: "stopped: the agent did not stop when asked, so Postil ended it".to_owned(),
            });
        }
        if let Some(thread) = &thread {
            self.forget_deleted_ask(&run, thread);
            self.end_analysis_thread(&run, thread);
        }
        (self.notify)();
        result
    }

    /// Answers run `id`'s question `request` (its number, as the log gives it) with the form's
    /// values, or with `None` to skip it.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchRequest`] when the run's agent is not running in this window, and as
    /// [`Agent::answer_question`].
    pub(crate) fn reply(
        &self,
        id: &str,
        request: usize,
        content: Option<serde_json::Map<String, Value>>,
    ) -> Result<()> {
        let mut slots = self.slots();
        let Some(Slot {
            run: Some(run),
            agent: Held::Ready { agent, .. },
            ..
        }) = slots.live_slot(id)
        else {
            return Err(Error::NoSuchRequest);
        };
        let reply = match &content {
            Some(content) => ElicitationReply::Accept {
                content: content.clone(),
            },
            None => ElicitationReply::Decline,
        };
        agent.answer_question(request, &reply)?;
        run.record(Entry::Reply { request, content });
        drop(slots);
        (self.notify)();
        Ok(())
    }

    /// Answers run `id`'s permission request `request` (its number, as the log gives it) with
    /// the option `option`.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchRequest`] when the run's agent is not running in this window, and as
    /// [`Agent::answer_permission`], which keeps the table of open requests.
    pub(crate) fn answer(&self, id: &str, request: usize, option: &str) -> Result<()> {
        let mut slots = self.slots();
        let Some(Slot {
            run: Some(run),
            agent: Held::Ready { agent, .. },
            ..
        }) = slots.live_slot(id)
        else {
            return Err(Error::NoSuchRequest);
        };
        // The agent checks the option against what it offered; only an accepted answer is
        // logged, so a refused one can be made again.
        agent.answer_permission(request, option)?;
        run.record(Entry::Answer {
            request,
            option: Some(option.to_owned()),
        });
        drop(slots);
        (self.notify)();
        Ok(())
    }

    /// The latest run, with the entries from number `after` on, and its notes and prompt only
    /// when `after` is 0: this window's latest run, or the newest readable log on disk when
    /// this window has run nothing yet. `None` when there is no run.
    ///
    /// # Errors
    ///
    /// [`Error::ReadRun`] when the runs directory cannot be listed.
    pub(crate) fn view(&self, after: usize) -> Result<Option<RunView>> {
        let run = self.slots().latest.clone();
        match run {
            Some(run) => Ok(Some(run.view(after))),
            None => Ok(Run::latest(&self.repo.runs_dir())?.map(|run| run.view(after))),
        }
    }

    /// The run `id` (`R2`), with the entries from number `after` on, and its notes and prompt
    /// only when `after` is 0: this window's when it is one of its threads' latest, else the
    /// run as its log on disk has it.
    ///
    /// # Errors
    ///
    /// [`Error::BadRequest`] when `id` is not of the form `R<number>`; [`Error::NoSuchRun`]
    /// when its log is missing or cannot be read.
    pub(crate) fn view_of(&self, id: &str, after: usize) -> Result<RunView> {
        Ok(self.run_by_id(id)?.view(after))
    }

    /// What team memory is recalled for in the dialog of the thread that ends with run `id`
    /// (docs/review-assist.md, "Postil shows memory"): the files its runs edited, then what it
    /// was asked, its first prompt without the fixed words every prompt of notes starts with,
    /// and each follow-up. Takes no lock that outlives the call: the caller then waits on
    /// memory with none held.
    ///
    /// # Errors
    ///
    /// As [`Runner::view_of`].
    pub(crate) fn recall_query(&self, id: &str) -> Result<String> {
        let thread = self.thread(id)?;
        let mut seen = HashSet::new();
        let mut paths = Vec::new();
        for run in &thread {
            for path in run.state().edited_paths() {
                if seen.insert(path.clone()) {
                    paths.push(path);
                }
            }
        }
        let mut text = paths.join("\n");
        for run in &thread {
            let state = run.state();
            let prompt = state.header.prompt.as_str();
            let asked = match &state.header.follow_up {
                Some(follow_up) => follow_up.text.as_str(),
                None => notes_asked(prompt),
            };
            text.push_str("\n\n");
            text.push_str(asked.trim());
        }
        Ok(text.trim().to_owned())
    }

    /// This window's runs under way, oldest first, each as `(run id, thread id)`: the page
    /// follows each, and tells by its thread what it holds back.
    pub(crate) fn live(&self) -> Vec<(String, String)> {
        let mut live: Vec<(String, String)> = self
            .slots()
            .by_thread
            .iter()
            .filter_map(|(thread, slot)| Some((thread, slot.run.as_ref()?)))
            .filter_map(|(thread, run)| {
                let state = run.state();
                state
                    .live()
                    .then(|| (state.header.id.clone(), thread.clone()))
            })
            .collect();
        live.sort_by_key(|(id, _)| run_number(id));
        live
    }

    /// What the runs from `from` to `to` changed (docs/design.md, "The changes view"): the
    /// diff from the snapshot taken before `from` to the one taken after `to`. A run that has
    /// none after it (it is under way, ended before its agent took the turn, or its snapshot
    /// failed) is compared with the files as they are now, and `after` is `None`: the diff
    /// may then hold changes made since.
    ///
    /// # Errors
    ///
    /// As [`Runner::view_of`] for both runs; [`Error::BadRequest`] when `from` has no snapshot
    /// before it (it ran before Postil took them, or its snapshot failed, which its log says)
    /// or `to` is older than `from`; and the errors of [`Repo::snapshot`] and [`Repo::diff`].
    pub(crate) fn changes(&self, from: &str, to: &str) -> Result<Changes> {
        let (first, last) = (self.run_by_id(from)?, self.run_by_id(to)?);
        if run_number(to) < run_number(from) {
            return Err(Error::BadRequest(format!("{to} is older than {from}")));
        }
        let before = first
            .state()
            .snapshot(Point::Before)
            .map(str::to_owned)
            .ok_or_else(|| {
                Error::BadRequest(format!(
                    "{from} has no snapshot from before it: it ran before Postil took them, or \
                     its log says why one could not be taken"
                ))
            })?;
        let after = last.state().snapshot(Point::After).map(str::to_owned);
        let now = match &after {
            Some(after) => after.clone(),
            None => self.files_now(&last)?,
        };
        let patch = self.repo.diff(&before, &now)?;
        Ok(Changes {
            before,
            after,
            patch,
        })
    }

    /// A snapshot of the files `run` works on as they are now ([`Repo::snapshot`]): its
    /// thread's worktree, or the engineer's working tree for a run from before threads had
    /// worktrees. A run whose thread was forgotten, which also has no snapshot after it, is
    /// taken for one of those too, and compared with the engineer's files: accepted, as it
    /// takes a failed commit and a deleted log together, and the view says it compares with
    /// the files as they are.
    ///
    /// # Errors
    ///
    /// [`Error::BadRequest`] when the thread's worktree is gone, and as [`Runner::thread_of`]
    /// and [`Repo::snapshot`].
    fn files_now(&self, run: &Run) -> Result<String> {
        match self.thread_of(run)? {
            None => self.repo.snapshot(),
            Some(thread) if thread.state == State::Open && thread.path.is_dir() => {
                let home = self.home.as_deref().ok_or(Error::NoHome)?;
                thread::open(&self.repo, home, &thread)?.snapshot()
            }
            Some(thread) => Err(Error::BadRequest(format!(
                "{} has no snapshot after it, and the worktree of its thread {} is gone",
                run.state().header.id,
                thread.id
            ))),
        }
    }

    /// The thread run `run` belongs to: its own for a run of notes, the reviewed run's for a
    /// review stage, the first run's for a follow-up, and for a memory draft that of the turn
    /// it drafts after. `None` for a run from before threads had worktrees.
    ///
    /// # Errors
    ///
    /// As [`Runner::thread`] for a follow-up and a memory draft, and as [`thread::find`].
    fn thread_of(&self, run: &Run) -> Result<Option<Thread>> {
        let (id, review, follow_of, drafted) = {
            let state = run.state();
            let review = state.header.review.as_ref().map(|stage| stage.of.clone());
            let follow_of = state
                .header
                .follow_up
                .as_ref()
                .map(|follow| follow.of.clone());
            let drafted = state
                .header
                .remember
                .as_ref()
                .map(|remember| remember.of.clone());
            (state.header.id.clone(), review, follow_of, drafted)
        };
        let first = match (review, drafted, follow_of) {
            (Some(of), _, _) => of,
            (None, Some(of), _) | (None, None, Some(of)) => self.root_run_id(&of)?,
            (None, None, None) => id,
        };
        thread::find(&self.repo, &first)
    }

    /// Walks `header.follow_up.of` backwards using header-only reads to find the thread's first
    /// run id without parsing full `.jsonl` logs.
    fn root_run_id(&self, start: &str) -> Result<String> {
        let mut current = start.to_owned();
        let mut seen = HashSet::new();
        while seen.insert(current.clone()) {
            let header = self.header_by_id(&current)?;
            match header.follow_up {
                Some(follow_up) => current = follow_up.of,
                None => return Ok(header.id),
            }
        }
        Ok(current)
    }

    /// Ends the open thread `id` as `ending` says (docs/design.md, "Ending a thread"), and
    /// frees its worktree: renamed into the trash and deleted in the background. Its branch
    /// stays, until the thread's last note is deleted.
    ///
    /// Holds the thread's lock throughout, so no run, in this window or another, works in the
    /// worktree meanwhile, and reads the thread only once it holds it. First checks that the
    /// worktree holds nothing Postil did not commit, and that its branch is at the commit
    /// Postil left or past it (a commit the engineer made there). Apply writes the branch's
    /// changes, from the commit the thread started at to the worktree's commit, into the
    /// engineer's working tree, uncommitted, all or nothing. The thread's kept agent is stopped
    /// once the check passes, and for Apply once the changes are written. The thread is marked
    /// ended before its worktree is freed: a free
    /// that fails then leaves an ended thread, whose worktree goes with its last note, and
    /// never an open one whose changes were applied already, which a second Apply would
    /// write again, unless the record itself cannot be written: the thread then stays open,
    /// and the error says why.
    ///
    /// # Errors
    ///
    /// [`Error::NoHome`] without Postil's home; [`Error::TurnRunning`] while one of its runs
    /// is under way in this window, its review is between two stages, or its worktree is
    /// being made, and [`Error::RunElsewhere`] while another window runs one;
    /// [`Error::BadRequest`] when there is no open thread `id`, or for Apply on a thread that
    /// only reads; [`Error::ThreadChanged`] when the check fails; [`Error::ApplyConflict`] when
    /// the changes do not fit the working tree, which then stays as it was; as
    /// [`thread::update`]; and as [`thread::free`], once the thread is marked ended.
    pub(crate) fn end_thread(&self, id: &str, ending: Ending) -> Result<()> {
        let home = self.home.as_deref().ok_or(Error::NoHome)?;
        // While the thread is busy in this window, its run holds the thread's lock, or a
        // review between two stages is about to take it from the reader thread: either would
        // read as another window's. Not busy, nothing in this window takes that lock until
        // this request is answered (only a request starts a run in an idle thread), so the
        // slots are free to take again under it below.
        if self.slots().by_thread.get(id).is_some_and(Slot::busy) {
            return Err(Error::TurnRunning);
        }
        let _lock = hold_thread_lock(&self.repo.runs_dir(), id)?;
        let thread = match thread::find(&self.repo, id)? {
            // An evicted thread has its changes on its branch, and no worktree to check.
            Some(thread) if matches!(thread.state, State::Open | State::Evicted) => thread,
            Some(_) => return Err(Error::BadRequest(format!("thread {id} already ended"))),
            None => return Err(Error::BadRequest(format!("there is no thread {id}"))),
        };
        if ending == Ending::Apply && thread.mode == Mode::Read {
            return Err(Error::BadRequest(format!(
                "thread {id} only reads: it has no changes to apply"
            )));
        }
        thread::check_place(&self.repo, home, &thread)?;
        let tip = thread::check(&self.repo, home, &thread)?;
        if ending == Ending::Apply {
            let patch = self.repo.patch(&thread.fork, &tip)?;
            self.repo.apply(&patch).map_err(|error| match error {
                Error::Git { stderr, .. } => Error::ApplyConflict { stderr },
                error => error,
            })?;
        }
        let released = self
            .slots()
            .by_thread
            .remove(id)
            .map_or(Held::None, |mut slot| slot.release());
        // Dropping the agent waits for its process, so after the lock.
        drop(released);
        let ended = thread::update(&self.repo, id, |stored| {
            // Applied, the engineer's commits past Postil's are in their working tree, and
            // the branch may go with the thread's note. Discarded, they are nowhere else:
            // the tip stays Postil's, so deleting the note keeps a branch past it.
            if ending == Ending::Apply {
                stored.tip = tip;
            }
            stored.state = match ending {
                Ending::Apply => State::Applied,
                Ending::Discard => State::Discarded,
            };
        })
        .and_then(|()| thread::free(&self.repo, home, &thread))
        .map(|trashed| {
            if let Some(trashed) = trashed {
                trashed.delete_later();
            }
        });
        if ended.is_ok() {
            // The thread is Applied or Discarded and can never run again: clean up its lock file
            // while still holding `_lock` so `live_threads` does not scan dead lock files.
            remove_thread_lock(&self.repo.runs_dir(), id);
        }
        (self.notify)();
        ended
    }

    /// Ends, with nothing applied, every open or evicted thread that only reads and started
    /// from a run carrying note `id`: what resolving an ask does before the note is marked
    /// resolved, so its worktree is freed (docs/review-assist.md, "Where read-only agents run").
    /// A change note has no such thread, and nothing is done. Reads the record, and the header
    /// of each such thread's first run.
    ///
    /// # Errors
    ///
    /// As [`Runner::end_thread`]: [`Error::TurnRunning`] while a run of the thread is under
    /// way in this window and [`Error::RunElsewhere`] while another window's is, which keeps a
    /// live ask from being resolved (decided 2026-11-26); and as [`thread::all`].
    pub(crate) fn end_ask(&self, id: &str) -> Result<()> {
        let dir = self.repo.runs_dir();
        for thread in thread::all(&self.repo)? {
            let ends = thread.mode == Mode::Read
                && matches!(thread.state, State::Open | State::Evicted)
                && read_header(&dir.join(format!("{}.jsonl", thread.id)))
                    .is_ok_and(|header| header.notes.iter().any(|note| note == id));
            if ends {
                self.end_thread(&thread.id, Ending::Discard)?;
            }
        }
        Ok(())
    }

    /// When `run`, in `thread`, has ended in a thread that only reads and none of its notes is
    /// left (an ask deleted while it ran), deletes its log and forgets the thread now, as the
    /// delete would have had the run been over: the thread ends when the run does (decided
    /// 2026-11-26). Called wherever a run ends, once no slots guard is held: after an agent's
    /// event, after a Stop, and at the end of the start thread (a worktree that could not be
    /// made, a Stop while it was, an agent that did not start). A run that ends inside
    /// [`Runner::launch`] (its thread could not be placed, or no start thread could be
    /// spawned) goes at the next delete instead, as any run does.
    ///
    /// It runs on the agent's reader thread and the start thread too, where no request waits:
    /// forgetting takes the thread's lock and then the slots, as eviction does on a start
    /// thread (module docs, "Threads and locks"). Between the end being logged, which releases
    /// the thread's lock, and this cleanup, a request may start a follow-up in the thread: the
    /// thread is then busy, so forgetting leaves it, and that follow-up's own end comes back
    /// here. A follow-up asked for once forgetting holds the lock is refused, as during an
    /// ending. An error is left for the next delete, which tries again and reports it.
    fn forget_deleted_ask(&self, run: &Run, thread: &Thread) {
        let notes = {
            let state = run.state();
            // An analysis carries no notes, and its thread ends with it
            // ([`Runner::end_analysis_thread`]).
            if thread.mode != Mode::Read || state.live() || state.header.analysis.is_some() {
                return;
            }
            state.header.notes.clone()
        };
        let Ok(left) = notes::ids(&self.repo) else {
            return;
        };
        if notes.iter().all(|note| !left.contains(note)) {
            drop(self.delete_runs_without_notes());
        }
    }

    /// When `run`, in `thread`, is an analysis run that has ended, ends its thread
    /// (docs/review-assist.md, "Where read-only agents run"): lets its agent go, frees the
    /// worktree and deletes the branch ([`thread::forget`]), marks the thread discarded, and
    /// removes its lock file, which every count of live runs would try for as long as it
    /// stays. The record stays, with the commit the run started from, which an update compares
    /// the files with, until the run's log goes ([`Runner::delete_runs_without_notes`]). The
    /// slot goes last, once all that is done: a thread that cannot be ended (its lock held
    /// elsewhere, edits the undo could not put back) keeps its slot, so the run, which says
    /// why, is still the one this window shows. Called wherever a run ends, beside
    /// [`Runner::forget_deleted_ask`] and with no slots guard held: takes the thread's lock,
    /// then the slots, as forgetting a thread does; the thread is not busy once its only run
    /// ended, since an analysis takes no follow-up.
    fn end_analysis_thread(&self, run: &Run, thread: &Thread) {
        {
            let state = run.state();
            if state.header.analysis.is_none() || state.live() {
                return;
            }
        }
        let Some(home) = self.home.as_deref() else {
            return;
        };
        let dir = self.repo.runs_dir();
        let ended = (|| {
            let lock = hold_thread_lock(&dir, &thread.id)?;
            let released = self
                .slots()
                .by_thread
                .get_mut(&thread.id)
                .map_or(Held::None, Slot::release);
            // Dropping the agent waits for its process, so after the slots' lock.
            drop(released);
            // Read again under the lock, as ending a thread does.
            let stored = thread::find(&self.repo, &thread.id)?
                .filter(|stored| matches!(stored.state, State::Open | State::Evicted));
            if let Some(stored) = stored {
                thread::forget(&self.repo, home, &stored)?;
                thread::update(&self.repo, &thread.id, |stored| {
                    stored.state = State::Discarded;
                })?;
            }
            // Removed while it is held, as a new thread whose worktree failed removes its own.
            remove_thread_lock(&dir, &thread.id);
            drop(lock);
            Ok::<_, Error>(())
        })();
        match ended {
            Ok(()) => drop(self.slots().by_thread.remove(&thread.id)),
            Err(error) => {
                run.state().problem.get_or_insert(format!(
                    "the analysis's thread could not be ended, and its worktree stays: {}",
                    error_chain(&error)
                ));
                (self.notify)();
            }
        }
    }

    /// The name of the agent that ran run `id`: a follow-up goes to it.
    ///
    /// # Errors
    ///
    /// As [`Runner::view_of`].
    pub(crate) fn agent_of(&self, id: &str) -> Result<String> {
        Ok(self.header_by_id(id)?.agent)
    }

    /// The ids of the runs on disk, newest first: `["R3", "R2", "R1"]`.
    ///
    /// # Errors
    ///
    /// [`Error::ReadRun`] when the runs directory cannot be listed.
    pub(crate) fn ids(&self) -> Result<Vec<String>> {
        let dir = self.repo.runs_dir();
        let mut numbers =
            run_numbers(&dir).map_err(|source| Error::ReadRun { path: dir, source })?;
        numbers.sort_unstable_by(|a, b| b.cmp(a));
        Ok(numbers
            .into_iter()
            .map(|number| format!("R{number}"))
            .collect())
    }

    /// Deletes the log of every run none of whose notes is left: a run goes with its last
    /// note (docs/design.md, "Runs"). This window's
    /// live run is kept; it goes at a later delete once over. A run another window is running
    /// loses its log the same way; that window keeps writing to the deleted file, and once
    /// the run ends it shows there as the latest run until the project is reopened, and in no
    /// list.
    ///
    /// The notes are read after the logs are listed. A log is created only once its notes are
    /// saved (`send` reads them for the prompt first), so every listed log's notes are seen:
    /// another window's new note, even one given a freed id, keeps its new run.
    ///
    /// An analysis run carries no notes: its log goes once the run is over, in every window
    /// (its thread's lock is free), and no diagram of the analysis names it
    /// (`analysis::runs_named`): one that stored nothing, or whose diagrams were all replaced
    /// or deleted. Its thread's record goes with it, below.
    ///
    /// Then forgets every thread whose first run's log is gone and that is not busy in this
    /// window ([`Runner::forget_threads`]): takes its lock, stops its kept agent, frees its
    /// worktree, and deletes its branch while it is at Postil's last commit.
    ///
    /// # Errors
    ///
    /// [`Error::ReadRun`] when the runs directory cannot be listed, as [`notes::ids`] for the
    /// review file, [`Error::WriteRun`] when a log cannot be removed. Some logs may be removed
    /// by then; the rest are tried again at the next delete. Then as
    /// [`Runner::forget_threads`], once the logs are removed.
    pub(crate) fn delete_runs_without_notes(&self) -> Result<()> {
        let dir = self.repo.runs_dir();
        let numbers = run_numbers(&dir).map_err(|source| Error::ReadRun {
            path: dir.clone(),
            source,
        })?;
        let left = notes::ids(&self.repo)?;
        // An analysis run under way in any window holds its thread's lock, whose id is the
        // run's; no diagram names it until its merge. Read before the diagrams: a run not live
        // here has ended, so its merge is saved and the read below sees it. Either that cannot
        // be read keeps every analysis log.
        let running = held_threads(&dir).ok();
        let named = running
            .as_ref()
            .and_then(|_| analysis::runs_named(&self.repo).ok());
        // Read once: the slots are not held while logs are read, so the agents' events, which
        // need them, are never kept waiting on the disk.
        let live = self.live();
        let pipeline_ids = self.pipeline_run_ids();
        let mut deleted = Vec::new();
        for number in numbers {
            let id = format!("R{number}");
            if live.iter().any(|(run, _)| *run == id) || pipeline_ids.contains(&id) {
                continue;
            }
            let file = dir.join(format!("{id}.jsonl"));
            // A log that cannot be read names no notes this check can see: it is left alone,
            // and the panel says what is wrong with it.
            let Ok(header) = read_header(&file) else {
                continue;
            };
            // An analysis carries no notes: its log stays while its run is under way, in any
            // window, and then while a stored diagram names it, as their record.
            let kept = match &header.analysis {
                Some(_) => {
                    running
                        .as_ref()
                        .is_some_and(|running| running.contains(&header.id))
                        || named
                            .as_ref()
                            .is_none_or(|named| named.contains(&header.id))
                }
                None => header.notes.iter().any(|note| left.contains(note)),
            };
            if kept {
                continue;
            }
            fs::remove_file(&file).map_err(|source| Error::WriteRun { path: file, source })?;
            deleted.push(id);
        }
        {
            let mut slots = self.slots();
            let gone = |run: &Option<Arc<Run>>| {
                run.as_ref()
                    .is_some_and(|run| deleted.contains(&run.state().header.id))
            };
            if gone(&slots.latest) {
                // Not the newest log on disk: that one may be another thread's live run, read
                // without its lock as unfinished.
                slots.latest = slots.newest_live();
            }
            for slot in slots.by_thread.values_mut() {
                if gone(&slot.run) {
                    slot.run = None;
                }
            }
        }
        let forgotten = self.forget_threads();
        (self.notify)();
        forgotten
    }

    /// Forgets every thread whose first run's log is gone: frees its worktree, deletes its
    /// branch, and removes it from the record, and its lock file. Stops a kept agent that works
    /// in one. Only for a thread not busy in this window: holds its lock, then takes the slots,
    /// which is safe only while no run of this window can take that lock meanwhile. A thread
    /// whose lock another window's run holds is kept, and the next delete tries again; so is
    /// one whose follow-up still runs here when its first log went.
    ///
    /// Each thread goes through [`thread::forget`], which keeps what is not Postil's to delete:
    /// work Postil did not commit, a branch past Postil's commit or checked out, and any
    /// branch outside `postil/`.
    ///
    /// # Errors
    ///
    /// As [`thread::all`], [`thread::forget`] and [`thread::remove`]. A thread that fails one
    /// of them stays in the record, and is tried again at the next delete.
    fn forget_threads(&self) -> Result<()> {
        let Some(home) = self.home.as_deref() else {
            return Ok(());
        };
        let dir = self.repo.runs_dir();
        let gone: Vec<Thread> = thread::all(&self.repo)?
            .into_iter()
            .filter(|thread| !dir.join(format!("{}.jsonl", thread.id)).exists())
            .collect();
        let mut held = Vec::new();
        for thread in gone {
            // A thread busy in this window waits for a delete after it is done: its run holds
            // the thread's lock, or is about to. Not busy, no run of this window takes that
            // lock until this request is answered (module docs, "Threads and locks").
            if self
                .slots()
                .by_thread
                .get(&thread.id)
                .is_some_and(Slot::busy)
            {
                continue;
            }
            match hold_thread_lock(&dir, &thread.id) {
                Ok(lock) => held.push((thread, lock)),
                // Another window's run works there; the next delete tries again.
                Err(Error::RunElsewhere) => {}
                Err(error) => return Err(error),
            }
        }
        if held.is_empty() {
            return Ok(());
        }
        let released: Vec<Held> = {
            let mut slots = self.slots();
            held.iter()
                .filter_map(|(thread, _)| slots.by_thread.remove(&thread.id))
                .map(|mut slot| slot.release())
                .collect()
        };
        // Dropping the agents waits for their processes, so after the lock.
        drop(released);
        let mut first_error = None;
        let mut removed = Vec::new();
        let mut locks = Vec::new();
        for (thread, lock) in held {
            locks.push(lock);
            // In any state: an ended thread whose free failed still has its worktree, which
            // keeps the branch checked out.
            // A commit the engineer made past Postil's is on the thread's branch (`free` checks
            // the worktree has it checked out), which then stays: it is the engineer's work
            // now. The thread is forgotten and the branch is not, so no delete is refused for
            // good. Only uncommitted work in the worktree keeps the thread, which `free`
            // refuses to delete.
            match thread::forget(&self.repo, home, &thread) {
                Ok(()) => removed.push(thread.id),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        thread::remove(&self.repo, &removed)?;
        for id in &removed {
            remove_thread_lock(&dir, id);
        }
        // Released only now, so no window starts a run in a thread being forgotten.
        drop(locks);
        first_error.map_or(Ok(()), Err)
    }

    /// Whether this window has a run under way, a review between two stages, whose next
    /// stage starts with no request from the page, or a pull request being opened. Closing the
    /// project does not stop an opening: it finishes and records its URL.
    pub(crate) fn busy(&self) -> bool {
        // The slots are let go before `pull_work` is taken: the two are never held together.
        let running = self.slots().busy();
        running || self.opening_pull_request()
    }

    /// Stops every agent, running or starting, and ignores anything they would still report:
    /// the project is closing, or the app quits. A run under way ends as failed, saying so, so
    /// it does not read as another window's.
    pub(crate) fn close(&self) {
        let mut released = Vec::new();
        let mut runs = Vec::new();
        {
            let mut slots = self.slots();
            slots.closed = true;
            for slot in slots.by_thread.values_mut() {
                slot.chain = None;
                // A run whose worktree git is still checking out is left to the start thread,
                // which ends it once git is done: ending it here would release the thread's
                // lock that keeps every window out of the half-made worktree, and a commit
                // there would take a half checkout.
                if let Some(run) = slot.run.clone().filter(|_| !slot.making) {
                    runs.push((run, slot.thread.clone()));
                }
                released.push(slot.release());
            }
        }
        // The agents are gone first, so nothing they report lands after the end. `closed`
        // drops their events anyway.
        drop(released);
        for (run, thread) in runs {
            // An analysis merging its reply ends the run itself, as a forced Stop leaves it,
            // and ends its thread then.
            if run.claim_end() {
                // As a forced Stop does: the agent's exit is not reported, so the snapshot
                // after the run is taken here, or its changes view would stay empty. It runs
                // `git add` on the request thread, quitting included.
                undo_if_live(self, &run, Some(&thread));
                self.record_end_snapshot(&run, Some(&thread));
                run.record(Entry::Failed {
                    error: "stopped: Postil closed the project while it ran".to_owned(),
                });
            }
            self.end_analysis_thread(&run, &thread);
        }
    }

    fn slots(&self) -> MutexGuard<'_, Slots> {
        lock(&self.slots)
    }

    fn usage_sink(&self) -> Option<LedgerSink> {
        self.usage_file.as_ref().map(|file| LedgerSink {
            file: file.clone(),
            repo: self.repo.root().to_string_lossy().into_owned(),
        })
    }

    /// Starts the agent on its own thread, then sends the prompt, unless the agent was let go
    /// meanwhile (the run cancelled, the project closed). A new thread's worktree is made
    /// there first ([`thread::make`]), which takes as long as a checkout.
    ///
    /// The agent's session gets team memory when the run asks for it and hippius-mem is Ready
    /// ([`Memory::for_run`], which may wait for a check). Its prompt of notes has the line about
    /// team memory exactly when the session got the server, whether or not it was built with
    /// it ([`Prompt::for_memory`]); when it got none, the run's dialog says why, once. A review's
    /// first stage with team memory on recalls the review's memories first
    /// ([`Runner::recall_for_review`]).
    ///
    /// # Errors
    ///
    /// The system's error when that thread cannot be spawned; the caller records it in the
    /// run.
    fn start(self: &Arc<Self>, pending: Pending) -> std::io::Result<()> {
        let runner = Arc::clone(self);
        let (cleaner, run, thread) = (
            Arc::clone(self),
            Arc::clone(&pending.run),
            pending.thread.clone(),
        );
        let start_agent = move || {
            let mut pending = pending;
            let generation = pending.generation;
            let id = pending.thread.id.clone();
            if !runner.ready_for_agent(&mut pending) {
                return (runner.notify)();
            }
            let failed = |error: &Error| {
                pending.run.record(Entry::Failed {
                    error: error_chain(error),
                });
            };
            // Blocks until the login shell has said where programs are, a few seconds at most
            // and only once per app.
            let search_path = runner.agents.wait();
            let servers = mcp_servers(&pending.run, runner.memory.for_run(pending.memory));
            if runner.slots().let_go(&id, generation) {
                return;
            }
            let launch = Launch {
                command: &pending.agent.command,
                cwd: &pending.thread.path,
                search_path: &search_path,
                mcp_servers: &servers,
                timeout: START_TIMEOUT,
            };
            // Weak: the runner owns the agent, whose reader thread owns this callback.
            let events = Arc::downgrade(&runner);
            let thread = id.clone();
            let spawned = Agent::spawn(&launch, move |event| {
                on_event(&events, &thread, generation, event);
            });
            let starting = match spawned {
                Ok(starting) => starting,
                Err(error) => {
                    if !runner.slots().let_go(&id, generation) {
                        failed(&error);
                    }
                    return (runner.notify)();
                }
            };

            // Stoppable from here on; a cancel or close that came first wins, and dropping
            // `starting` stops it.
            {
                let mut slots = runner.slots();
                if slots.let_go(&id, generation) {
                    return;
                }
                let slot = slots.by_thread.get_mut(&id).expect("let_go found the slot");
                slot.agent = Held::Starting(starting.stopper());
            }
            let opened = starting.open_session(pending.agent.model.as_deref());

            let mut slots = runner.slots();
            if slots.let_go(&id, generation) {
                // The run already says why. An agent that started after all is stopped by
                // dropping it, after the lock: dropping it waits for its process.
                drop(slots);
                drop(opened);
                return;
            }
            let slot = slots.by_thread.get_mut(&id).expect("let_go found the slot");
            match opened {
                Err(error) => {
                    slot.agent = Held::None;
                    failed(&error);
                }
                Ok(Opened {
                    agent,
                    servers_refused,
                }) => {
                    // The line goes with the server, whatever the state was when the prompt was
                    // built: a check may have ended Ready meanwhile, or the agent refused it.
                    let given = if !servers.is_empty() && servers_refused.is_none() {
                        TeamMemory::On
                    } else {
                        TeamMemory::Off
                    };
                    if let Some(refused) = servers_refused {
                        let notice = format!("team memory was not given: {refused}");
                        first_memory_notice(&pending.run, notice);
                    }
                    let prompt = pending.prompt.for_memory(given);
                    let root = &pending.thread.path;
                    begin(&agent, &pending.run, &pending.ids, &prompt, root);
                    slot.agent = Held::Ready {
                        choice: pending.agent,
                        agent,
                        memory: given,
                    };
                }
            }
            drop(slots);
            (runner.notify)();
        };
        let start = move || {
            start_agent();
            // Every slots guard is dropped by now. A run that ended here (its worktree could not
            // be made, a Stop came while it was, its agent did not start) ends a deleted ask's
            // thread as an agent's last event does; one that goes on returns at once.
            cleaner.forget_deleted_ask(&run, &thread);
            cleaner.end_analysis_thread(&run, &thread);
        };
        std::thread::Builder::new()
            .name("agent-start".to_owned())
            .spawn(start)
            .map(drop)
    }

    /// Makes the worktree of `pending`'s new or evicted thread on its start thread, once the
    /// disk has room for it ([`Runner::free_disk`]), then marks the slot done making. Returns
    /// whether the agent is to start: not when the worktree could not be made, nor when the run
    /// was stopped or the project closed meanwhile; the run then ends here, saying why, and
    /// with it the thread's lock.
    fn make_for(&self, pending: &Pending) -> bool {
        let id = &pending.thread.id;
        let made = self.make_worktree(pending);
        let (gone, closed) = {
            let mut slots = self.slots();
            if let Some(slot) = slots.by_thread.get_mut(id) {
                slot.making = false;
                // A remade worktree is on its branch's commit, which a commit the engineer
                // made there before the eviction moved past the recorded tip: a proposer's
                // undo goes back to it.
                if let Ok(Some(tip)) = &made {
                    slot.thread.tip.clone_from(tip);
                    slot.thread.state = State::Open;
                }
            }
            (slots.let_go(id, pending.generation), slots.closed)
        };
        if gone {
            pending.run.record(if closed {
                Entry::Failed {
                    error: "stopped: Postil closed the project while it ran".to_owned(),
                }
            } else {
                Entry::Ended {
                    reason: StopReason::Cancelled,
                }
            });
            return false;
        }
        let tip = match made {
            Ok(tip) => tip,
            Err(error) => {
                pending.run.record(Entry::Failed {
                    error: error_chain(&error),
                });
                return false;
            }
        };
        let before = match (&pending.worktree, tip) {
            // A new thread's run is a run of notes, which takes snapshots unless its thread
            // only reads: the one before is the commit the worktree was made at.
            (Worktree::Make { snapshot }, _) => snapshot.then(|| pending.thread.fork.clone()),
            (Worktree::Remake { snapshot }, tip) => tip.filter(|_| *snapshot),
            (Worktree::Made, _) => None,
        };
        if let Some(before) = before {
            pending.run.record_snapshot(Point::Before, before);
        }
        true
    }

    /// Readies `pending` on its start thread, before its agent starts: makes its worktree when
    /// it is still to be made ([`Runner::make_for`]), then, for a review's first stage with
    /// team memory on, recalls the review's memories ([`Runner::recall_for_review`]). Returns
    /// whether the agent is to start, as `make_for` does.
    fn ready_for_agent(&self, pending: &mut Pending) -> bool {
        if !matches!(pending.worktree, Worktree::Made) && !self.make_for(pending) {
            return false;
        }
        if let Some(snapshots) = pending.review_recall.take()
            && pending.memory == TeamMemory::On
        {
            self.recall_for_review(pending, &snapshots);
        }
        true
    }

    /// Recalls the team memories the review whose first stage is `pending` checks the work
    /// against (docs/review-assist.md, "Review checks against memory"), for the reviewed run's
    /// prompt and each path it changed: its edits' diffs, then the paths that differ between its
    /// `snapshots`, which a command's edits are among. The chain keeps them for every stage, and
    /// the stage's prompt is built again with them. When none can be read, the stage runs as it
    /// was, and says why as its run's memory notice, never as its problem: the run did not fail.
    ///
    /// Runs on the stage's start thread, with no lock held while git runs and while it waits
    /// for hippius-mem ([`Memory::recall_review`], up to 30 s). A Stop meanwhile lets the stage
    /// go, and the memories are dropped.
    fn recall_for_review(&self, pending: &mut Pending, snapshots: &ReviewSnapshots) {
        let id = &pending.thread.id;
        let (asked, files) = {
            let slots = self.slots();
            if slots.let_go(id, pending.generation) {
                return;
            }
            let Some(chain) = slots.by_thread.get(id).and_then(|slot| slot.chain.as_ref()) else {
                return;
            };
            (
                notes_asked(&chain.work.prompt).trim().to_owned(),
                chain.files.clone(),
            )
        };
        let found = self
            .review_queries(asked, files, snapshots)
            .and_then(|queries| self.memory.recall_review(queries));
        let block = match found {
            Ok(found) => review::MemoryBlock::new(found),
            Err(why) => {
                pending.run.state().memory_notice =
                    Some(format!("team memory could not be checked: {why}"));
                return;
            }
        };
        if block.is_empty() {
            return;
        }
        let mut slots = self.slots();
        if slots.let_go(id, pending.generation) {
            return;
        }
        let Some(chain) = slots
            .by_thread
            .get_mut(id)
            .and_then(|slot| slot.chain.as_mut())
        else {
            return;
        };
        // Only the first stage recalls (`Runner::review`), and the first of a chain proposes.
        debug_assert!(
            !chain.stage.judges(),
            "a review's memories are recalled for its first stage, which proposes"
        );
        chain.work.memories = block;
        pending.prompt.text = review::propose_prompt(&chain.work);
    }

    /// What a review recalls for: the reviewed run's notes as asked (`asked`, left out when
    /// empty), then each path it changed once, `files` (its edits' diffs) first, then the
    /// paths that differ between its snapshots: a command's edits, which report no diff to
    /// Postil, are among them. A renamed file is its old path and its new one.
    ///
    /// # Errors
    ///
    /// Why the paths that differ between `snapshots` could not be read ([`Repo::changed_paths`]).
    fn review_queries(
        &self,
        asked: String,
        files: Vec<String>,
        snapshots: &ReviewSnapshots,
    ) -> std::result::Result<Vec<String>, String> {
        let mut seen: std::collections::HashSet<String> = files.iter().cloned().collect();
        let mut paths = files;
        paths.truncate(MAX_REVIEW_QUERY_PATHS);
        if paths.len() < MAX_REVIEW_QUERY_PATHS
            && let (Some(before), Some(after)) = (&snapshots.before, &snapshots.after)
            && before != after
        {
            let changed = self.repo.changed_paths(before, after).map_err(|error| {
                format!(
                    "the reviewed run's changed paths could not be read: {}",
                    error_chain(&error)
                )
            })?;
            for changed in changed {
                if paths.len() >= MAX_REVIEW_QUERY_PATHS {
                    break;
                }
                if seen.insert(changed.path.clone()) {
                    paths.push(changed.path);
                }
            }
        }
        let mut queries = Vec::with_capacity(paths.len() + 1);
        if !asked.is_empty() {
            queries.push(asked);
        }
        queries.extend(paths);
        Ok(queries)
    }

    /// Takes new thread `id`, whose worktree will not be made, out of the record, and removes
    /// its lock file, which no record then names: nothing would ever forget it. A record that
    /// cannot be changed stays as it was; the run says why the thread failed. The caller's run
    /// still holds the thread's lock: a window that took it meanwhile would lock the file
    /// being removed, and a third one a new file, both working in the thread.
    fn drop_new_thread(&self, id: &str) {
        drop(thread::remove(&self.repo, &[id.to_owned()]));
        remove_thread_lock(&self.repo.runs_dir(), id);
    }

    /// Makes the worktree of `pending`'s new or evicted thread ([`thread::make`]) once the disk
    /// has room for it ([`Runner::free_disk`]), and records in the thread what it was not
    /// given; a record that cannot be written loses that list only. Returns, for an evicted
    /// thread, the commit its branch is at, which becomes its tip.
    ///
    /// A new thread whose worktree cannot be made goes from the record: it claims a branch that
    /// `git worktree add` may not have made, which a thread of another working tree of the
    /// repository may own, and deleting the thread's last note would delete it. An evicted
    /// thread is marked open before its worktree is made, as a new thread is written to the
    /// record before, so a crash leaves a record naming whatever was made; marked evicted
    /// again when it cannot be made.
    fn make_worktree(&self, pending: &Pending) -> Result<Option<String>> {
        #[cfg(test)]
        self.pass_making_gate();
        let thread = &pending.thread;
        let remake = matches!(pending.worktree, Worktree::Remake { .. });
        let mark =
            |state: State| thread::update(&self.repo, &thread.id, |stored| stored.state = state);
        let made = self
            .free_disk(&thread.id)
            .and_then(|()| if remake { mark(State::Open) } else { Ok(()) })
            .and_then(|()| thread::make(&self.repo, thread));
        let skipped = match made {
            Ok(skipped) => skipped,
            Err(error) => {
                if remake {
                    drop(mark(State::Evicted));
                } else {
                    self.drop_new_thread(&thread.id);
                }
                return Err(error);
            }
        };
        // A remade worktree replaces the list its first one left.
        if remake || !skipped.is_empty() {
            drop(thread::update(&self.repo, &thread.id, |stored| {
                stored.skipped = skipped;
            }));
        }
        if !remake {
            return Ok(None);
        }
        // Commits nothing in a fresh checkout: reads the commit it is at, and records it as the
        // tip, as every run of an open thread starts ([`Runner::place`]).
        self.commit_worktree(&pending.run, thread, Point::Before)
            .map(Some)
    }

    /// Commits everything in `thread`'s worktree for `run` at `point` ([`Repo::commit_all`])
    /// and returns the commit the worktree is then at, which becomes the thread's tip: a
    /// snapshot is a commit on the thread's branch. Before a run too, since a commit of work
    /// left in the worktree moves the branch, and a run that then never starts takes no
    /// snapshot after it.
    ///
    /// # Errors
    ///
    /// [`Error::NoHome`]; as [`thread::open`]; the errors of [`Repo::commit_all`], the work then
    /// left uncommitted in the worktree; and of [`thread::update`], the commit then made but the
    /// record's tip left behind it.
    fn commit_worktree(&self, run: &Run, thread: &Thread, point: Point) -> Result<String> {
        let message = commit_message(&run.state().header, point);
        let home = self.home.as_deref().ok_or(Error::NoHome)?;
        let commit = thread::open(&self.repo, home, thread)?.commit_all(&message)?;
        thread::update(&self.repo, &thread.id, |stored| {
            stored.tip.clone_from(&commit);
        })?;
        Ok(commit)
    }

    /// Records the snapshot after `run`, in `thread`, as its turn ends: only while it is live,
    /// has the one before and not yet one after, so an agent that exits after its run ended
    /// adds none. The first caller claims it under the run's state, so a forced Stop and the
    /// turn's end arriving together commit once.
    fn record_end_snapshot(&self, run: &Run, thread: Option<&Thread>) {
        let Some(thread) = thread else {
            return;
        };
        let wanted = {
            let mut state = run.state();
            let wanted = state.live()
                && !state.after_claimed
                && state.snapshot(Point::Before).is_some()
                && state.snapshot(Point::After).is_none();
            state.after_claimed |= wanted;
            wanted
        };
        if wanted {
            match self.commit_worktree(run, thread, Point::After) {
                Ok(commit) => run.record_snapshot(Point::After, commit),
                Err(error) => no_snapshot(run, &error),
            }
        }
    }

    /// Writes a new thread in `mode` for run `run`'s record, starting at commit `fork` and named
    /// from `title`.
    fn new_thread(&self, run: &Run, fork: &str, title: &str, mode: Mode) -> Result<Thread> {
        let id = run.state().header.id.clone();
        let mut thread = thread::plan(&self.repo, self.home.as_deref(), &id, fork, title)?;
        thread.mode = mode;
        thread::add(&self.repo, &thread)?;
        Ok(thread)
    }

    /// Marks the notes `ids` sent in `run`. A note that cannot be marked would be sent again
    /// by mistake later, so the run says so.
    fn mark_sent(&self, run: &Run, ids: &[String]) {
        let id = run.state().header.id.clone();
        if let Err(error) = notes::mark_sent(&self.repo, ids, &id) {
            run.state().problem.get_or_insert(format!(
                "the notes were sent but could not be marked sent: {}",
                error_chain(&error)
            ));
        }
    }
}

/// The gate of [`Runner::hold_merge`]: what releases the merge, and where it says it is held.
#[cfg(test)]
#[derive(Debug)]
struct MergeGate {
    gate: std::sync::mpsc::Receiver<()>,
    held: std::sync::mpsc::Sender<()>,
}

#[cfg(test)]
impl Runner {
    /// Whether a review is under way: decided as each stage ends, before its end is logged.
    pub(crate) fn reviewing(&self) -> bool {
        self.slots()
            .by_thread
            .values()
            .any(|slot| slot.chain.is_some())
    }

    /// Holds the next review stage after the current one ends, until the returned sender
    /// sends or is dropped. Once: the stage after it starts as usual.
    pub(crate) fn hold_next_stage(&self) -> std::sync::mpsc::Sender<()> {
        let (release, gate) = std::sync::mpsc::channel();
        *lock(&self.next_stage_gate) = Some(gate);
        release
    }

    /// Waits at the gate [`Runner::hold_next_stage`] set, if any.
    fn pass_next_stage_gate(&self) {
        let gate = lock(&self.next_stage_gate).take();
        if let Some(gate) = gate {
            // A dropped sender lets the stage go as well.
            gate.recv().ok();
        }
    }

    /// Holds the next worktree's making, before `git worktree add`, until the returned sender
    /// sends or is dropped. Once.
    pub(crate) fn hold_making(&self) -> std::sync::mpsc::Sender<()> {
        let (release, gate) = std::sync::mpsc::channel();
        *lock(&self.making_gate) = Some(gate);
        release
    }

    /// Holds the next analysis merge once it claimed the run's end, until the returned sender
    /// sends or is dropped; the returned receiver gets a message once the merge is held. Once.
    pub(crate) fn hold_merge(
        &self,
    ) -> (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>) {
        let (release, gate) = std::sync::mpsc::channel();
        let (held, reached) = std::sync::mpsc::channel();
        *lock(&self.merge_gate) = Some(MergeGate { gate, held });
        (release, reached)
    }

    /// Waits at the gate [`Runner::hold_merge`] set, if any.
    fn pass_merge_gate(&self) {
        let gate = lock(&self.merge_gate).take();
        if let Some(MergeGate { gate, held }) = gate {
            // The test may have stopped waiting; the merge goes on as well then.
            held.send(()).ok();
            gate.recv().ok();
        }
    }

    /// Waits at the gate [`Runner::hold_making`] set, if any.
    fn pass_making_gate(&self) {
        let gate = lock(&self.making_gate).take();
        if let Some(gate) = gate {
            // A dropped sender lets the making go as well.
            gate.recv().ok();
        }
    }

    /// Moves every live run's first Stop back by [`FORCE_AFTER`], so a test can force a stop
    /// without waiting that long.
    pub(crate) fn backdate_stop(&self) {
        let slots = self.slots();
        for run in slots
            .by_thread
            .values()
            .filter_map(|slot| slot.run.as_ref())
        {
            let mut state = run.state();
            state.cancel_asked = state.cancel_asked.map(|at| {
                at.checked_sub(FORCE_AFTER)
                    .expect("the monotonic clock has run longer than FORCE_AFTER")
            });
        }
    }
}

impl fmt::Debug for Runner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Runner")
            .field("root", &self.repo.root())
            .field("slots", &*self.slots())
            .finish_non_exhaustive()
    }
}

/// An agent's event, on its reader thread: recorded in the current run unless the agent
/// was let go. An agent that exited, or whose turn failed, is let go: the next send starts a
/// fresh one, since a failed turn can mean a broken session (an adapter whose backend died,
/// a sign-in that expired), and a lost conversation costs less than a stuck one.
#[expect(
    clippy::too_many_lines,
    reason = "coordinates slot inspection, lock-free read-only undo, turn finish, and logging"
)]
fn on_event(runner: &Weak<Runner>, id: &str, generation: u64, event: Event) {
    let Some(runner) = runner.upgrade() else {
        return;
    };
    let ended_well = matches!(
        event,
        Event::TurnEnded(TurnEnd::Stopped {
            reason: StopReason::EndTurn,
            ..
        })
    );
    let ends_turn = matches!(event, Event::TurnEnded(_) | Event::Exited { .. });
    let ends_agent = matches!(
        event,
        Event::Exited { .. } | Event::TurnEnded(TurnEnd::Failed(_))
    );
    let (run, thread, root_path, answer, stage, analyzing, drafting, should_undo) = {
        let mut slots = runner.slots();
        if slots.let_go(id, generation) {
            return;
        }
        let slot = slots.by_thread.get_mut(id).expect("let_go found the slot");
        // Avoid cloning the entire `Thread` (6+ Strings) on every streamed token chunk; only
        // clone it when the turn ends and snapshots/undo/cleanup need the full record.
        let root_path = slot.thread.path.clone();
        let thread = ends_turn.then(|| slot.thread.clone());
        let (review, read_only_thread, analyzing, drafting) =
            slot.run
                .as_ref()
                .map_or((None, false, false, false), |run| {
                    let state = run.state();
                    (
                        state.header.review.clone(),
                        state.header.read_only,
                        state.header.analysis.is_some(),
                        state.header.remember.is_some(),
                    )
                });
        // Answered before the request is logged, and logged with it at once: a page that read
        // the request alone would show the run waiting for the engineer.
        // From the run's header, which never changes: a Stop that ends the chain does not lift
        // it for the rest of the proposer's turn.
        let read_only = read_only_thread || review.as_ref().is_some_and(|stage| !stage.judges());
        // The review stage this run is, while its review goes on.
        let stage = review.filter(|stage| {
            slot.chain
                .as_ref()
                .is_some_and(|chain| chain.stage == *stage)
        });
        let answer = auto_answer(&slot.agent, &event, read_only);
        // A stage that does not end well ends its review.
        if ends_turn && !ended_well && stage.is_some() {
            slot.chain = None;
        }
        let run = slot.run.clone();
        let should_undo =
            ends_turn && read_only && run.as_deref().is_some_and(|run| run.state().live());
        (
            run,
            thread,
            root_path,
            answer,
            stage,
            analyzing,
            drafting,
            should_undo,
        )
    };
    // Run `undo_read_only` outside `runner.slots()` so 4-6 synchronous git subprocesses never
    // stall concurrent API requests or other live agents. The run is still live and still holds
    // its thread lock file, so no other run can enter the worktree before `Entry::Ended` is logged.
    let undone = if should_undo {
        run.as_deref()
            .and_then(|run| undo_read_only(&runner, run, thread.as_ref()))
    } else {
        None
    };
    // An analysis that changed files stores nothing: its tree is not the one it was given.
    let edited = undone.is_some();
    let (released, next) = if ends_turn || ends_agent {
        let Some(finished) = finish_turn_slot(
            &runner,
            id,
            generation,
            ends_turn,
            ends_agent,
            ended_well,
            analyzing,
            drafting,
            stage.as_ref(),
            run.as_ref(),
            undone,
        ) else {
            return;
        };
        finished
    } else {
        (Held::None, None)
    };
    if let Some(run) = &run {
        // The agent names files in the worktree it works in.
        let root = root_path.as_path();
        // Before the end is logged, so a reader that sees the end sees what the run changed,
        // and its report.
        if ends_turn {
            runner.record_end_snapshot(run, thread.as_ref());
        }
        // Read before the end is logged, which releases the thread's lock: the worktree is
        // still the run's.
        let before_end = match (&thread, ended_well && !edited) {
            (Some(thread), true) if analyzing => finish_analysis(&runner, run, thread),
            (_, true) => report_entry(run, root),
            (_, false) => None,
        };
        if let Some(ids) = run.record_event(event, before_end, answer, root) {
            runner.mark_sent(run, &ids);
        }
        if ends_turn && let Some(thread) = &thread {
            runner.forget_deleted_ask(run, thread);
        }
    }
    // Dropped outside the lock: dropping an agent waits for its process to be reaped.
    drop(released);
    if ends_turn && let (Some(run), Some(thread)) = (&run, &thread) {
        runner.end_analysis_thread(run, thread);
    }
    (runner.notify)();
    if let Some(ended) = next {
        #[cfg(test)]
        runner.pass_next_stage_gate();
        runner.advance(id, &ended);
    } else if ends_turn && let Some(ended) = &run {
        runner.advance_pipeline(id, ended, ended_well && !edited);
    }
}

/// Releases the slot's agent if needed and decides whether a review chain advances to its next
/// stage, under `runner.slots()` after any read-only undo has finished outside the lock.
#[expect(
    clippy::too_many_arguments,
    reason = "internal helper split out of `on_event` to keep `on_event` readable"
)]
#[expect(
    clippy::fn_params_excessive_bools,
    reason = "mirrors the turn-end flags already computed in `on_event`"
)]
fn finish_turn_slot(
    runner: &Runner,
    id: &str,
    generation: u64,
    ends_turn: bool,
    ends_agent: bool,
    ended_well: bool,
    analyzing: bool,
    drafting: bool,
    stage: Option<&Stage>,
    run: Option<&Arc<Run>>,
    undone: Option<String>,
) -> Option<(Held, Option<Arc<Run>>)> {
    let mut slots = runner.slots();
    if slots.let_go(id, generation) {
        return None;
    }
    let slot = slots.by_thread.get_mut(id).expect("let_go found the slot");
    // An analysis's agent goes with its turn: its thread ends with the run. So does a memory
    // draft's: the draft is the slot's run now, so no turn could reuse its agent.
    let released = if ends_agent || (ends_turn && (analyzing || drafting)) {
        slot.release()
    } else {
        Held::None
    };
    // Re-verify `slot.chain` under the lock: a non-forced `Runner::cancel()` during unlocked
    // `undo_read_only` may have cleared `slot.chain` without releasing the slot.
    let stage = stage.filter(|stage| {
        slot.chain
            .as_ref()
            .is_some_and(|chain| chain.stage == **stage)
    });
    // Decided under the lock, before the stage's end is logged: a reader that sees the end
    // sees the decision, and no Stop or new review can replace the chain meanwhile. The
    // next stage starts once the end is logged.
    let next = match (stage, run) {
        (Some(stage), Some(run)) if ended_well => {
            decide(slot, run, stage, undone).then(|| Arc::clone(run))
        }
        (_, Some(run)) => {
            // No findings are logged for this end: the run says it, in its log, before the
            // end.
            if let Some(text) = undone {
                run.record(Entry::Undone { text });
            }
            None
        }
        _ => None,
    };
    Some((released, next))
}

/// Ends review stage `stage` of `run`, whose turn ended well in `thread`'s worktree, and decides
/// whether the review goes on: true when the next stage is to start, else false, the chain
/// then cleared. A proposer's findings are logged here, before the end, while the run still
/// takes entries.
fn decide(slot: &mut Slot, run: &Run, stage: &Stage, undone: Option<String>) -> bool {
    let none = review::MemoryBlock::default();
    let (found, memories) = slot.chain.as_ref().map_or((0, &none), |chain| {
        (chain.findings.len(), &chain.work.memories)
    });
    let findings = end_stage(run, stage, found, memories, undone);
    let goes = match (findings, slot.chain.as_mut()) {
        (Some(findings), Some(chain)) => goes_on(chain, findings),
        _ => false,
    };
    if !goes {
        slot.chain = None;
    }
    goes
}

/// Answers `event` when it is a permission request to `held`, a ready agent, as Postil does on
/// the engineer's behalf (docs/design.md, "Runs", decided with the owner on 2026-11-23: only a
/// question stops for the engineer), and returns the entry that logs the answer. A run that
/// may only read (a proposing reviewer's, a memory draft's, or one in a thread that only
/// reads) is kept so: only its reads, searches, fetches and thinking are allowed. `None` for
/// any other event, when no option fits, or when a Stop already answered the request (and
/// logged that) or the agent is gone; the run then shows the request open until it ends.
fn auto_answer(held: &Held, event: &Event, read_only: bool) -> Option<Entry> {
    let (Event::Permission(request), Held::Ready { agent, .. }) = (event, held) else {
        return None;
    };
    let reads = matches!(
        request.tool_call.kind,
        Some(ToolKind::Read | ToolKind::Search | ToolKind::Fetch | ToolKind::Think)
    );
    // Once, else always. With neither offered the request waits for the engineer.
    let kinds = if read_only && !reads {
        [PermissionKind::RejectOnce, PermissionKind::RejectAlways]
    } else {
        [PermissionKind::AllowOnce, PermissionKind::AllowAlways]
    };
    let option = first_option(&request.options, kinds)?;
    agent.answer_permission(request.number, &option).ok()?;
    Some(Entry::Answer {
        request: request.number,
        option: Some(option),
    })
}

/// Moves `chain` to its next stage, given the findings of the stage that ended. Returns
/// whether there is one to start: not past the last reviewer, and not a judge with nothing
/// to judge.
fn goes_on(chain: &mut Chain, findings: Vec<Finding>) -> bool {
    chain.findings.extend(findings);
    chain.stage.reviewer += 1;
    chain.stage.reviewer <= chain.stage.reviewers
        && !(chain.stage.judges() && chain.findings.is_empty())
}

/// Ends review stage `stage` of `run`, whose turn ended well, before its end is logged: a
/// proposer's findings are read from its reply (numbered after the `found` before it, a memory
/// kept only when `memories`, the stage's block, held it) and logged. `undone` says why the
/// proposer's edits were put back ([`undo_read_only`]), if it made any. Returns the findings for
/// the next stage; `None` when the review ends here: this was the judge, or a proposer changed
/// files.
fn end_stage(
    run: &Run,
    stage: &Stage,
    found: usize,
    memories: &review::MemoryBlock,
    undone: Option<String>,
) -> Option<Vec<Finding>> {
    if stage.judges() {
        return None;
    }
    let reply = run.state().reply();
    let (findings, problem) = match review::findings_in(&reply, found + 1, memories) {
        Ok(findings) => (findings, None),
        Err(problem) => (Vec::new(), Some(problem)),
    };
    // Edits end the review, so no judge takes a changed tree for the reviewed one.
    let edited = undone.is_some();
    run.record(Entry::Findings {
        findings: findings.clone(),
        problem: undone.or(problem),
    });
    (!edited).then_some(findings)
}

/// [`undo_read_only`] for a run a forced Stop or a close ends, whose end the agent does not
/// report: only while it is live, and the run logs what was undone.
fn undo_if_live(runner: &Runner, run: &Run, thread: Option<&Thread>) {
    if !run.state().live() {
        return;
    }
    if let Some(text) = undo_read_only(runner, run, thread) {
        run.record(Entry::Undone { text });
    }
}

/// When `run` may only read (a proposing reviewer's, a memory draft's in any thread, or any run
/// of a thread that only reads) and changed `thread`'s worktree, puts the worktree back to the
/// thread's branch at its tip ([`Repo::reset_to`], ignored files kept) and returns what the run
/// says about it; `None` for any other run, or one that changed nothing. Called however the
/// turn ends, a Stop and a close included: edits left there would be taken in by the next run's
/// commit, and by Apply with it.
///
/// Such a run may only read, but an agent that edits without asking is not stopped by the
/// permission rule. An edit made through a command reports no diff, one it committed leaves
/// no change, and one on another branch leaves `HEAD` anywhere, so the worktree is asked for
/// all three: a change, another branch checked out, or the branch moved. A worktree that
/// cannot be asked counts as changed. Runs git in the worktree, opened by [`thread::open`]:
/// `status`, `branch`, `rev-parse`, and the reset when needed.
fn undo_read_only(runner: &Runner, run: &Run, thread: Option<&Thread>) -> Option<String> {
    // Who changed files: a proposing reviewer, by its place in the review, or the agent of a
    // read-only run: a run of a thread that only reads, or a memory draft in any thread.
    let reviewer = {
        let state = run.state();
        match &state.header.review {
            Some(stage) if !stage.judges() => Some(stage.reviewer),
            _ if state.header.read_only => None,
            _ => return None,
        }
    };
    let thread = thread?;
    let home = runner.home.as_deref()?;
    let said = |undone: String| {
        Some(match reviewer {
            Some(reviewer) => format!(
                "reviewer {reviewer} changed files, though it may only read: {undone}, and \
                 the review stops here"
            ),
            None => format!("the agent changed files, though it may only read: {undone}"),
        })
    };
    let tree = match thread::open(&runner.repo, home, thread) {
        Ok(tree) => tree,
        Err(error) => {
            return said(format!(
                "its worktree could not be opened: {}",
                error_chain(&error)
            ));
        }
    };
    let moved = (|| {
        Ok::<_, Error>(
            !tree.changed()?.is_empty()
                || tree.branch()? != thread.branch
                || tree.head()?.as_deref() != Some(thread.tip.as_str()),
        )
    })()
    .unwrap_or(true);
    if !moved && !run.state().has_completed_diffs() {
        return None;
    }
    match tree.reset_to(&thread.branch, &thread.tip) {
        Ok(()) => said("its changes were undone".to_owned()),
        Err(error) => said(format!(
            "its changes could not be undone: {}",
            error_chain(&error)
        )),
    }
}

/// The id of the first of `options` of the first kind in `kinds` that has one; `None` when
/// none of `options` is of any of them.
fn first_option(options: &[PermissionOption], kinds: [PermissionKind; 2]) -> Option<String> {
    kinds
        .into_iter()
        .find_map(|kind| options.iter().find(|option| option.kind == kind))
        .map(|option| option.option_id.clone())
}

/// The agent `slots` keep for the thread `first` when it ran `followed`, the slot's run, and
/// still runs: the one a follow-up or a memory draft of `followed` goes on with. An evicted
/// thread's agent, kept in another window, worked in a worktree that is gone. The kept agent
/// goes on with its own choice, model included, whatever the settings now say: a slot changed
/// since must not drop the conversation or switch its model (decided 2026-11-26). Only a fresh
/// agent takes the settings' choice.
fn kept_agent(slots: &Slots, first: &str, evicted: bool, followed: &Arc<Run>) -> Option<Kept> {
    slots
        .by_thread
        .get(first)
        .filter(|_| !evicted)
        .filter(|slot| {
            slot.run
                .as_ref()
                .is_some_and(|run| Arc::ptr_eq(run, followed))
        })
        .and_then(|slot| match &slot.agent {
            // The slot's run is `followed`, and the caller names the agent from that run, so
            // the kept agent is always the run's own agent: only whether it still runs is
            // checked.
            Held::Ready {
                choice,
                agent,
                memory,
            } if agent.is_alive() => Some(Kept {
                choice: choice.clone(),
                memory: *memory,
            }),
            Held::Ready { .. } | Held::Starting(_) | Held::None => None,
        })
}

/// A thread's kept agent, as [`kept_agent`] finds it: its choice, and whether its session got
/// hippius-mem's server.
struct Kept {
    choice: AgentChoice,
    memory: TeamMemory,
}

/// The files a fresh agent's prompt links in a thread in `mode`: every file its runs edited.
/// Edits a read-only run made were undone: there is no file to link for them.
fn thread_files(thread: &[Arc<Run>], mode: Mode) -> Vec<String> {
    match mode {
        Mode::Work => {
            let mut seen = std::collections::HashSet::new();
            let mut files = Vec::new();
            for run in thread {
                for path in run.state().edited_paths() {
                    if seen.insert(path.clone()) {
                        files.push(path);
                    }
                }
            }
            files
        }
        Mode::Read => Vec::new(),
    }
}

/// What a memory draft asks before the note it drafts from (docs/review-assist.md, "Postil
/// writes memory"). The owner chose a draft over the note's raw text: a note written for one
/// agent in one moment does not stand on its own. The block's shape is what `run/log.rs`
/// (`draft_of`) reads.
const DRAFT_ASK: &str = "The engineer wants to record what this work taught as a team memory: \
     a note that teammates and their agents recall later, without this conversation. Draft it \
     from the note below and what you know of the work on it.\nDo not edit, create or delete \
     any file, do not run a command that changes one, do not commit, and do not call any tool \
     that writes team memory: the engineer reads your draft, edits it, and saves it.\nWrite one \
     self-contained fact: a decision, a convention, a gotcha, a reference, or context, with its \
     reason and where it applies. End your reply with a ```json block holding one object: \
     {\"summary\": \"one line, with the words a teammate would search for\", \"body\": \"the \
     fact in full\"}.\n";

/// The prompt of a memory draft about `note`, quoted as a prompt quotes a note, to the agent
/// that ran the thread.
fn draft_ask(note: &str) -> String {
    format!("{DRAFT_ASK}\n{note}")
}

/// The prompt of a memory draft about `note` to a fresh agent in a thread in `mode`: the
/// thread so far ([`thread_so_far`]), then [`draft_ask`].
fn fresh_draft(thread: &[Arc<Run>], note: &str, mode: Mode) -> String {
    let mut prompt = thread_so_far(thread, mode);
    write!(prompt, "\n{}", draft_ask(note)).expect("writing to a String cannot fail");
    prompt
}

/// The prompt of a follow-up that goes to a fresh agent in a thread in `mode`: the thread so
/// far ([`thread_so_far`]), then the engineer's `text`, then the ask for a report
/// ([`report_ask`]), or, in a thread that only reads, the rule that nothing is changed
/// ([`notes::ASK_RULES`]).
fn fresh_follow_up(thread: &[Arc<Run>], text: &str, mode: Mode) -> String {
    let mut prompt = thread_so_far(thread, mode);
    let ending = match mode {
        Mode::Work => report_ask(&thread[0].state().header.prompt),
        Mode::Read => notes::ASK_RULES,
    };
    write!(prompt, "\nThe engineer now asks:\n\n{text}\n\n{ending}")
        .expect("writing to a String cannot fail");
    prompt
}

/// What a fresh agent is told of a thread in `mode` before what it is asked: each run's prompt
/// and reply in order. The diffs stay out: the agent reads the files as they are now, linked
/// after the text.
fn thread_so_far(thread: &[Arc<Run>], mode: Mode) -> String {
    // The page tells a fresh follow-up's prompt from a kept agent's by its start
    // (`ui/status.js`, `freshAgent`), whatever the sentence.
    let mut prompt = String::from(match mode {
        Mode::Work => {
            "You are continuing work another coding agent did in this repository; its \
             conversation is gone. Here is what it was asked and what it answered, oldest \
             first. Read the files for their current state.\n"
        }
        Mode::Read => {
            "You are continuing a conversation another coding agent had about this \
             repository; it is gone. Here is what it was asked and what it answered, oldest \
             first. Read the files for their current state.\n"
        }
    });
    for run in thread {
        let state = run.state();
        // Without the line about team memory, an instruction to that agent, which this one
        // may not have.
        let asked = state.header.follow_up.as_ref().map_or_else(
            || notes::without_team_memory(&state.header.prompt),
            |follow_up| follow_up.text.clone(),
        );
        write!(
            prompt,
            "\n{} was asked:\n\n{}\n\nIt answered:\n\n{}\n",
            state.header.id,
            asked.trim_end(),
            state.reply().trim()
        )
        .expect("writing to a String cannot fail");
    }
    prompt
}

/// What ends a fresh agent's follow-up prompt, given the prompt the thread started with: a
/// reminder of the report that prompt asked for, or the whole ask when it holds no
/// `report::ASK` as worded now (a thread from before reports, run log version 5 and older, or
/// from before a change to the wording).
fn report_ask(first_prompt: &str) -> &'static str {
    if first_prompt.contains(report::ASK) {
        report::ASK_AGAIN
    } else {
        report::ASK
    }
}

/// The entry that logs the author's report of `run`, a turn that ended well, read from its
/// reply in `root` (`report::read`); `None` for a review stage, which logs findings instead,
/// and for a run that only reads, which was asked for none. The caller logs it with the run's
/// end, under one hold of the run: a forced stop or a close that ends the run first takes
/// neither. Asks the file system whether each step's path exists, so it is never called under
/// the `Slots` lock.
fn report_entry(run: &Run, root: &Path) -> Option<Entry> {
    let reply = {
        let state = run.state();
        // A run that a forced stop or a close ended takes no entry: no file is read for it.
        if state.header.review.is_some() || state.header.read_only || !state.live() {
            return None;
        }
        state.reply()
    };
    Some(match report::read(&reply, root) {
        Ok(report) => Entry::Report { report },
        Err(problem) => Entry::NoReport { problem },
    })
}

/// Merges the reply of `run`, an analysis whose turn ended well in `thread`'s worktree, into
/// the analysis ([`analysis::merge`]), and returns the entry that says why nothing was stored,
/// if nothing was; `None` once stored, and for a run that is not an analysis or whose end a
/// forced stop or a close claimed first ([`Run::claim_end`]). Its own claim keeps those from
/// ending the run, and so from freeing the worktree, while it reads there. The caller logs it
/// with the run's end, while the run still holds its thread's lock, so no window starts a run
/// in the worktree meanwhile. Reads every
/// cited file in the worktree and takes `analysis.lock`, so it is never called under the
/// `Slots` lock.
fn finish_analysis(runner: &Runner, run: &Run, thread: &Thread) -> Option<Entry> {
    let (asked, id, reply) = {
        let state = run.state();
        (
            state.header.analysis.clone()?,
            state.header.id.clone(),
            state.reply(),
        )
    };
    // A forced Stop or a close that claimed the end first ends the run: nothing is stored.
    if !run.claim_end() {
        return None;
    }
    #[cfg(test)]
    runner.pass_merge_gate();
    // A run has a thread only with a home: `thread::plan` refuses one without.
    let home = runner.home.as_deref()?;
    let problem = match thread::open(&runner.repo, home, thread) {
        Ok(tree) => analysis::merge(&runner.repo, &tree, &id, &asked, &reply).err()?,
        Err(error) => format!(
            "its worktree could not be opened, so nothing was stored: {}",
            error_chain(&error)
        ),
    };
    Some(Entry::NoAnalysis { problem })
}

/// Says on `run` that no snapshot was taken, and why: the run goes on, without its changes
/// view ([`Runner::commit_worktree`] says what became of the work).
fn no_snapshot(run: &Run, error: &Error) {
    run.state().problem.get_or_insert(format!(
        "no snapshot of the files was taken: {}",
        error_chain(error)
    ));
}

/// The message of the commit a snapshot at `point` makes for the run `header` describes: what
/// was asked, so the branch reads as the thread's history. A commit before a run takes only
/// what was left in the worktree since the last one (a commit that failed, or a hand edit).
fn commit_message(header: &Header, point: Point) -> String {
    let id = &header.id;
    if point == Point::Before {
        return format!("Postil: work left in the worktree before {id}");
    }
    match (&header.review, &header.follow_up) {
        (Some(stage), _) => format!(
            "Postil {id}: reviewer {} of {} on {}",
            stage.reviewer, stage.reviewers, stage.of
        ),
        (None, Some(follow_up)) => format!(
            "Postil {id}: follow-up of {}\n\n{}\n",
            follow_up.of,
            follow_up.text.trim_end()
        ),
        (None, None) => format!(
            "Postil {id}: notes {}\n\n{}\n",
            header.notes.join(", "),
            header.prompt.trim_end()
        ),
    }
}

/// What a prompt of notes asks, without the words every such prompt starts with: the
/// introduction, the line about team memory and the ask for a report. A prompt that does not
/// start with them is given whole.
fn notes_asked(prompt: &str) -> &str {
    prompt
        .strip_prefix(notes::INTRO)
        .map(|rest| rest.strip_prefix(notes::TEAM_MEMORY).unwrap_or(rest))
        .and_then(|rest| rest.strip_prefix(report::ASK))
        .unwrap_or(prompt)
}

/// The files `diffs` edit, each once, in the order first edited.
fn edited_files(diffs: &[Diff]) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    for diff in diffs {
        if !files.contains(&diff.path) {
            files.push(diff.path.clone());
        }
    }
    files
}

/// Sends the prompt to a running agent. Its notes are marked sent once the agent shows it
/// took the turn ([`Run::record_event`]).
///
/// A kept agent can send an update for its previous turn after replying to it. One that
/// arrives after this prompt was sent counts as the sign, though the agent may not have
/// read the prompt yet: ACP's updates carry no turn, so the two cannot be told apart.
/// The risk is accepted. The reader reads each line as the agent writes it, and a send
/// takes a request from the page, so only an update the agent holds back after its reply
/// can land here.
///
/// The prompt links files in `root`, the worktree the agent works in.
fn begin(agent: &Agent, run: &Run, ids: &[String], prompt: &Prompt, root: &Path) {
    run.set_model(agent.model());
    if !ids.is_empty() {
        run.state().unmarked = Some(ids.to_vec());
    }
    let blocks = prompt_blocks(prompt, root);
    if let Err(error) = agent.prompt(&blocks) {
        return run.record(Entry::Failed {
            error: error_chain(&error),
        });
    }
    run.record(Entry::Started);
}

/// The MCP servers `memory` gives `run`'s agent. A notice that it gives none goes on `run` at
/// once, so a run that then fails to start says it too: the notice is given once
/// ([`Memory::for_run`]), unless the run has one already ([`first_memory_notice`]).
fn mcp_servers(run: &Run, memory: RunMemory) -> Vec<McpServer> {
    match memory {
        RunMemory::Server(server) => vec![server],
        RunMemory::None { notice } => {
            if let Some(notice) = notice {
                first_memory_notice(run, notice);
            }
            Vec::new()
        }
    }
}

/// Gives `run` the memory notice `notice` unless it has one: the first stays. A review's
/// recall that failed comes before the start finds no server or the agent refuses it, and says
/// why the review has no memories.
fn first_memory_notice(run: &Run, notice: String) {
    run.state().memory_notice.get_or_insert(notice);
}

/// The prompt as ACP content: the text, then a link to each file with notes. Every agent
/// accepts both block types, so no capability check is needed.
fn prompt_blocks(prompt: &Prompt, root: &Path) -> Vec<ContentBlock> {
    let links = prompt.files.iter().map(|path| ContentBlock::ResourceLink {
        uri: file_uri(&root.join(path)),
        name: path.clone(),
    });
    std::iter::once(ContentBlock::Text {
        text: prompt.text.clone(),
    })
    .chain(links)
    .collect()
}

/// A `file://` URI for an absolute path, with every byte outside RFC 3986's unreserved set
/// and `/` percent-encoded.
fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_encoded_bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

/// Locks a runner's or a run's state. Poisoning is ignored, by choice. A change under these
/// locks can set several fields (`RunState::apply` and `Run::write` update the summary and
/// the entries; a send sets the slot's agent, run and chain), so a panic midway can leave
/// them out of step. Treating the lock as poisoned instead would make every later request
/// that touches the project panic too; a run shown out of step is the smaller harm.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh agent sees the thread's first prompt as history: a reminder suffices when that
    /// prompt asked for the report, and a thread from before reports needs the whole ask.
    #[test]
    fn a_fresh_follow_up_of_a_thread_from_before_reports_gets_the_whole_ask() {
        let asked = format!("Address every note.\n{}\n[N1] a.txt", report::ASK);
        assert_eq!(report_ask(&asked), report::ASK_AGAIN);
        assert_eq!(report_ask("Address every note.\n\n[N1] a.txt"), report::ASK);
    }

    #[test]
    fn a_chain_s_debug_output_counts_its_findings_and_never_shows_their_text() {
        let chain = Chain {
            stage: Stage {
                of: "R1".to_owned(),
                reviewer: 2,
                reviewers: 2,
            },
            reviewers: Vec::new(),
            work: Work {
                prompt: "secret note".to_owned(),
                reply: "secret reply".to_owned(),
                diffs: Vec::new(),
                report: None,
                memories: review::MemoryBlock::new(vec![review::MemoryNote {
                    id: "mem_01A".to_owned(),
                    kind: "gotcha".to_owned(),
                    author: "5CrAuthor".to_owned(),
                    summary: "secret summary".to_owned(),
                    body: "secret body".to_owned(),
                }]),
            },
            notes: vec!["N1".to_owned()],
            files: Vec::new(),
            findings: vec![Finding {
                id: "F1".to_owned(),
                note: None,
                path: None,
                line: None,
                text: "secret finding".to_owned(),
                fix: Some("secret fix".to_owned()),
                memory: Some("mem_01A".to_owned()),
            }],
            memory: TeamMemory::Off,
        };
        let shown = format!("{chain:?}");
        assert!(
            shown.contains("reviewer: 2") && shown.contains("findings: 1"),
            "{shown}"
        );
        assert!(!shown.contains("secret"), "{shown}");
    }
}
