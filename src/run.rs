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
//! milliseconds on a medium repository, as a snapshot is), and a proposer's check and undo as
//! its stage ends (`status`, `branch`, `rev-parse`, `checkout -B`, `clean`), each after the
//! few `rev-parse` calls that open the worktree ([`thread::open`]). Every page request, and
//! every other thread's agent events, wait for those: one lock for all threads keeps the lock
//! order above, and costs milliseconds.
//! The page is told about every change through `notify`, and fetches what it has not seen.

mod log;
mod pull;
mod room;

#[cfg(test)]
pub(crate) use room::{FLOOR, TestDisk};

use std::collections::HashMap;
use std::fmt::{self, Write as _};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::acp::{
    Agent, ContentBlock, ElicitationReply, Event, Launch, PermissionKind, PermissionOption,
    PermissionRequest, StopReason, Stopper, ToolKind, TurnEnd,
};
use crate::agents::{AgentChoice, Scanner};
use crate::error::{Error, Result, error_chain};
use crate::notes::{self, Prompt};
use crate::repo::Repo;
use crate::report;
use crate::review::{self, Diff, Finding, Stage, Work};
use crate::run::log::{
    Changes, Entry, FollowUp, Header, InThread, MAX_LIVE, Opening, Origin, Point, Run, RunView,
    hold_thread_lock, read_header, remove_thread_lock, run_number, run_numbers,
};
use crate::thread::{self, State, Thread};
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

/// The runs of the open project, and the agent kept between them.
pub(crate) struct Runner {
    repo: Arc<Repo>,
    agents: Scanner,
    notify: Notify,
    /// Postil's home folder, `~/.postil`, under which threads' worktrees live
    /// (`thread.rs`); `None` when the home folder is unknown, which refuses every send.
    home: Option<PathBuf>,
    /// Durable global usage ledger (`<config dir>/usage.jsonl`), when configured.
    usage_file: Option<PathBuf>,
    slots: Mutex<Slots>,
    /// What this window is doing about each thread's pull request, by thread id
    /// ([`pull::PullWork`]); a lock never held with another.
    pull_work: Mutex<HashMap<String, pull::PullWork>>,
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
    Ready(AgentChoice, Agent),
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

    /// The live run of this window started most recently, if any: the latest run once the
    /// one before is gone.
    fn newest_live(&self) -> Option<Arc<Run>> {
        self.by_thread
            .values()
            .filter_map(|slot| slot.run.as_ref())
            .filter(|run| run.state().live())
            .max_by_key(|run| run_number(&run.state().header.id))
            .cloned()
    }

    /// Whether any thread of this window is busy ([`Slot::busy`]).
    fn busy(&self) -> bool {
        self.by_thread.values().any(Slot::busy)
    }

    /// Whether `thread` holds a live ready agent matching `choice`.
    fn has_live_agent(&self, thread: &str, choice: &AgentChoice) -> bool {
        self.by_thread.get(thread).is_some_and(|slot| {
            matches!(&slot.agent, Held::Ready(kept, live) if kept == choice && live.is_alive())
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

/// A send that waits for its agent to start.
#[derive(Debug)]
struct Pending {
    agent: AgentChoice,
    run: Arc<Run>,
    ids: Vec<String>,
    prompt: Prompt,
    generation: u64,
    /// The thread the agent works in.
    thread: Thread,
    worktree: Worktree,
}

/// Whether a run's worktree is still to be made, on the start thread, before its agent.
#[derive(Debug)]
enum Worktree {
    /// A new thread's, on a new branch: a worktree that cannot be made takes the thread out
    /// of the record.
    Make,
    /// An evicted thread's, again from its branch. The run records the commit it starts from
    /// as its snapshot before when `snapshot` (it is not a proposer).
    Remake {
        snapshot: bool,
    },
    Made,
}

/// A run for [`Runner::launch`] to start.
#[derive(Debug)]
struct NewRun {
    agent: AgentChoice,
    /// The notes in the run's header: it goes with the last of them.
    notes: Vec<String>,
    prompt: Prompt,
    origin: Origin,
    conversation: Conversation,
    /// The review the run is a stage of, which becomes the one under way; `None` ends the
    /// review under way, if any.
    review: Option<Chain>,
    place: Place,
}

/// Which thread a launched run works in.
#[derive(Debug)]
enum Place {
    /// A new thread, started at commit `fork` and named from `title`, its first note's text.
    New { fork: String, title: String },
    /// The open thread of this id, whose worktree is on disk: read again by `launch` once it
    /// holds the thread's lock, as ending a thread does, so another window cannot end or forget
    /// it in between.
    Open(String),
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
    pub(crate) fn new(
        repo: Arc<Repo>,
        agents: Scanner,
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
        Arc::new(Self {
            repo,
            agents,
            notify,
            home,
            usage_file,
            slots: Mutex::new(Slots::default()),
            pull_work: Mutex::default(),
            #[cfg(test)]
            next_stage_gate: Mutex::new(None),
            #[cfg(test)]
            making_gate: Mutex::new(None),
            #[cfg(test)]
            test_disk: Mutex::default(),
        })
    }

    /// The prompt the notes `ids` make, as the agent will receive it: its text and the files
    /// linked after it.
    ///
    /// # Errors
    ///
    /// As [`notes::prompt`].
    pub(crate) fn preview(&self, ids: &[String]) -> Result<Prompt> {
        notes::prompt(&self.repo, ids)
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
    pub(crate) fn send(self: &Arc<Self>, agent: AgentChoice, ids: &[String]) -> Result<()> {
        let slots = self.slots();
        // A note is marked sent only once its agent takes the turn, so until then it reads as
        // open: a second send would put it in two threads at once.
        if let Some((note, run)) = slots.carrying(ids) {
            return Err(Error::BadRequest(format!(
                "{note} is in {run}, which is under way: wait for it, or stop it"
            )));
        }
        let prompt = notes::prompt(&self.repo, ids)?;
        let fork = self.repo.head()?.ok_or(Error::NoCommit)?;
        let title = notes::body(&self.repo, &ids[0])?;
        let new = NewRun {
            agent,
            notes: ids.to_vec(),
            prompt,
            origin: Origin::Notes,
            // A new thread works in a new worktree, which a kept agent's session is not in.
            conversation: Conversation::Fresh,
            review: None,
            place: Place::New { fork, title },
        };
        self.launch(slots, new)
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
                return Err(Error::BadRequest(format!(
                    "the worktree of {id}'s thread is gone ({}): discard the thread, and send \
                     its notes again",
                    thread.path.display()
                )));
            }
            State::Applied => "applied",
            State::Discarded => "discarded",
        };
        Err(Error::BadRequest(format!(
            "{id}'s thread was {ended}: send its notes again to work on them"
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
    /// text, a run that is a review, or a run another run already follows; as
    /// [`Runner::view_of`] for `of` and the runs it follows, and [`Error::NoSuchRun`] when one
    /// follows a run that is not older; [`Error::ReadRun`] when the runs directory cannot be
    /// listed; [`Error::WriteRun`] when the run log cannot be created; and as
    /// [`Runner::open_thread`] for the thread, and [`Runner::launch`].
    pub(crate) fn follow_up(
        self: &Arc<Self>,
        of: &str,
        agent: AgentChoice,
        text: &str,
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
        let evicted = self.open_thread(&first)?.state == State::Evicted;
        let slots = self.slots();
        // The thread's kept agent ran `followed`, the slot's run. An evicted thread's agent,
        // kept in another window, worked in a worktree that is gone.
        let same_conversation = !evicted
            && slots.by_thread.get(&first).is_some_and(|slot| {
                matches!(
                    &slot.agent,
                    Held::Ready(kept, live) if *kept == agent && live.is_alive()
                ) && slot
                    .run
                    .as_ref()
                    .is_some_and(|run| Arc::ptr_eq(run, followed))
            });
        let prompt = if same_conversation {
            // After the words, so the page tells this prompt from a fresh agent's by its start
            // (`ui/status.js`, `freshAgent`).
            Prompt {
                text: format!("{text}\n\n{}", report::ASK_AGAIN),
                files: Vec::new(),
            }
        } else {
            let diffs: Vec<Diff> = thread
                .iter()
                .flat_map(|run| run.state().work().diffs)
                .collect();
            Prompt {
                text: fresh_follow_up(&thread, text),
                files: edited_files(&diffs),
            }
        };
        let new = NewRun {
            agent,
            notes,
            prompt,
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
            place: Place::Open(first),
        };
        self.launch(slots, new)
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
    /// reviewers, for a run that is itself a review, or for a follow-up; as
    /// [`Runner::view_of`] for the reviewed run; [`Error::WriteRun`] when the first stage's
    /// log cannot be created; and as [`Runner::open_thread`] for the thread, and
    /// [`Runner::launch`].
    pub(crate) fn review(self: &Arc<Self>, of: &str, reviewers: Vec<AgentChoice>) -> Result<()> {
        if reviewers.is_empty() {
            return Err(Error::BadRequest(
                "no reviewers are set: add them in Settings".to_owned(),
            ));
        }
        let reviewed = self.run_by_id(of)?;
        let (notes, work, files) = {
            let state = reviewed.state();
            if state.header.review.is_some() {
                return Err(Error::BadRequest(format!("{of} is itself a review")));
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
            (state.header.notes.clone(), work, files)
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
        };
        self.start_stage(self.slots(), chain)
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
        if !file.exists() {
            return Err(no_such("there is no such run log".to_owned()));
        }
        Run::read(&file).map(Arc::new).map_err(no_such)
    }

    /// Starts `chain`'s current stage as a new run, with a fresh agent, so a reviewer never
    /// inherits the reviewed agent's conversation; `chain` becomes the review under way. The
    /// stage works in the reviewed run's thread, read again for each stage.
    fn start_stage(self: &Arc<Self>, slots: MutexGuard<'_, Slots>, chain: Chain) -> Result<()> {
        // Refused here, before a run log is made; `launch` reads it again under the thread's
        // lock.
        self.open_thread(&chain.stage.of)?;
        let text = if chain.stage.judges() {
            review::judge_prompt(&chain.work, &chain.findings)
        } else {
            review::propose_prompt(&chain.work)
        };
        let new = NewRun {
            agent: chain.reviewers[chain.stage.reviewer - 1].clone(),
            notes: chain.notes.clone(),
            prompt: Prompt {
                text,
                files: chain.files.clone(),
            },
            origin: Origin::Review(chain.stage.clone()),
            conversation: Conversation::Fresh,
            place: Place::Open(chain.stage.of.clone()),
            review: Some(chain),
        };
        self.launch(slots, new)
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
        if let Err(error) = self.start_stage(slots, chain) {
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
    /// latest run, sends the run to its agent, then tells the page. The thread is placed as
    /// [`Runner::place`] says. A new thread's snapshot before, the commit it starts from, is
    /// recorded on the start thread once its worktree is made ([`Runner::start`]). The slot
    /// changes only once the run's log exists, so a launch that fails leaves the review and the
    /// kept agent as they were. The agents it replaces or lets go are dropped after `slots` is
    /// released.
    ///
    /// # Errors
    ///
    /// [`Error::TurnRunning`] while a run of the thread is under way in this window, as
    /// [`Run::create`]; and as [`Runner::place`], which fails the run too.
    fn launch(self: &Arc<Self>, mut slots: MutexGuard<'_, Slots>, new: NewRun) -> Result<()> {
        // A worktree still being made counts as a run under way: its agent has not started.
        // Between two review stages the chain has been taken by `advance`, which launches the
        // next one, so the thread is not busy for it.
        if let Place::Open(id) = &new.place
            && slots.by_thread.get(id).is_some_and(Slot::busy)
        {
            return Err(Error::TurnRunning);
        }
        let NewRun {
            agent,
            notes,
            prompt,
            origin,
            conversation,
            review,
            place,
        } = new;
        let snapshot = !matches!(&origin, Origin::Review(stage) if !stage.judges());
        // A follow-up and a review mark no note sent: the notes went to the run they follow or
        // review.
        let marks = match origin {
            Origin::Notes => notes.clone(),
            Origin::Review(_) | Origin::FollowUp(_) => Vec::new(),
        };
        let in_thread = match &place {
            Place::New { .. } => InThread::New,
            Place::Open(id) => InThread::Open(id),
        };
        let opening = Opening {
            agent: &agent.name,
            notes: &notes,
            prompt: &prompt.text,
            origin,
        };
        let run = Arc::new(Run::create(
            &self.repo.runs_dir(),
            opening,
            in_thread,
            self.usage_sink(),
        )?);
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
        slots.latest = Some(Arc::clone(&run));
        let slot = slots
            .by_thread
            .entry(id.clone())
            .or_insert_with(|| Slot::new(thread.clone()));
        slot.chain = review;
        let new_thread = matches!(worktree, Worktree::Make);
        slot.making = !matches!(worktree, Worktree::Made);
        let prev_session_cost = reuse.then(|| slot.run.as_deref()?.session_cost()).flatten();
        slot.run = Some(Arc::clone(&run));
        slot.thread = thread.clone();
        slot.used = used;
        let replaced = if reuse && let Held::Ready(_, kept) = &slot.agent {
            run.set_cost_baseline(prev_session_cost);
            begin(kept, &run, &marks, &prompt, &thread.path);
            Held::None
        } else {
            // The thread's agent kept from its last run, if any, is stopped when this is
            // dropped.
            let replaced = slot.release();
            let pending = Pending {
                agent,
                run: Arc::clone(&run),
                ids: marks,
                prompt,
                generation: slot.generation,
                thread,
                worktree,
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

    /// The thread `run` works in, as `place` says, and whether its worktree is still to be
    /// made. Called while `run` holds the thread's lock, so the thread is read under it. In an
    /// open thread, first commits what was left in the worktree and takes that commit as the
    /// snapshot before the run, when `snapshot`.
    ///
    /// # Errors
    ///
    /// For a new thread as [`thread::plan`] and [`thread::add`], the thread's lock file then
    /// removed; for an open one as [`Runner::open_thread`]; and without `snapshot` (a
    /// proposer), as [`Runner::commit_worktree`].
    fn place(&self, run: &Run, place: Place, snapshot: bool) -> Result<(Thread, Worktree)> {
        let (mut thread, worktree) = match place {
            Place::New { fork, title } => match self.new_thread(run, &fork, &title) {
                Ok(thread) => (thread, Worktree::Make),
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
            Place::Open(id) => {
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
            Worktree::Make | Worktree::Remake { .. } => {}
            // Every run of an open thread starts from a commit of what was left there, a
            // proposer's too: undoing a proposer's edits goes back to that commit, which must
            // hold the engineer's own commits and edits.
            Worktree::Made => match self.commit_worktree(run, &thread, Point::Before) {
                Ok(commit) => {
                    thread.tip.clone_from(&commit);
                    if snapshot {
                        run.record_snapshot(Point::Before, commit);
                    }
                }
                // A proposer whose start could not be marked would be blamed, and undone, for
                // what was left: it does not start.
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
                if let Held::Ready(_, agent) = &slot.agent {
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
        // Dropping the agent kills it and waits for its process to be reaped.
        drop(released);
        if forced {
            // Its exit is not reported, so the snapshot after it is taken here, once nothing
            // can write any more. The run stays live until then, so a send is refused
            // (`TurnRunning`) meanwhile.
            undo_if_live(self, &run, thread.as_ref());
            self.record_end_snapshot(&run, thread.as_ref());
            run.record(Entry::Failed {
                error: "stopped: the agent did not stop when asked, so Postil ended it".to_owned(),
            });
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
            agent: Held::Ready(_, agent),
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
            agent: Held::Ready(_, agent),
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
    /// review stage, the first run's for a follow-up. `None` for a run from before threads
    /// had worktrees.
    ///
    /// # Errors
    ///
    /// As [`Runner::thread`] for a follow-up, and as [`thread::find`].
    fn thread_of(&self, run: &Run) -> Result<Option<Thread>> {
        let (id, review, follows) = {
            let state = run.state();
            let review = state.header.review.as_ref().map(|stage| stage.of.clone());
            (
                state.header.id.clone(),
                review,
                state.header.follow_up.is_some(),
            )
        };
        let first = match review {
            Some(of) => of,
            None if follows => self.thread(&id)?[0].state().header.id.clone(),
            None => id,
        };
        thread::find(&self.repo, &first)
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
    /// [`Error::BadRequest`] when there is no open thread `id`; [`Error::ThreadChanged`] when
    /// the check fails; [`Error::ApplyConflict`] when the changes do not fit the working tree,
    /// which then stays as it was; as [`thread::update`]; and as [`thread::free`], once the
    /// thread is marked ended.
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
        (self.notify)();
        ended
    }

    /// The name of the agent that ran run `id`: a follow-up goes to it.
    ///
    /// # Errors
    ///
    /// As [`Runner::view_of`].
    pub(crate) fn agent_of(&self, id: &str) -> Result<String> {
        Ok(self.run_by_id(id)?.state().header.agent.clone())
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
        // Read once: the slots are not held while logs are read, so the agents' events, which
        // need them, are never kept waiting on the disk.
        let live = self.live();
        let mut deleted = Vec::new();
        for number in numbers {
            let id = format!("R{number}");
            if live.iter().any(|(run, _)| *run == id) {
                continue;
            }
            let file = dir.join(format!("{id}.jsonl"));
            // A log that cannot be read names no notes this check can see: it is left alone,
            // and the panel says what is wrong with it.
            let Ok(header) = read_header(&file) else {
                continue;
            };
            if header.notes.iter().any(|note| left.contains(note)) {
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
            // As a forced Stop does: the agent's exit is not reported, so the snapshot after
            // the run is taken here, or its changes view would stay empty. It runs `git add`
            // on the request thread, quitting included. Both calls do nothing for a run that
            // already ended.
            undo_if_live(self, &run, Some(&thread));
            self.record_end_snapshot(&run, Some(&thread));
            run.record(Entry::Failed {
                error: "stopped: Postil closed the project while it ran".to_owned(),
            });
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
    /// # Errors
    ///
    /// The system's error when that thread cannot be spawned; the caller records it in the
    /// run.
    fn start(self: &Arc<Self>, pending: Pending) -> std::io::Result<()> {
        let runner = Arc::clone(self);
        let start = move || {
            let generation = pending.generation;
            let id = pending.thread.id.clone();
            let failed = |error: &Error| {
                pending.run.record(Entry::Failed {
                    error: error_chain(error),
                });
            };
            if !matches!(pending.worktree, Worktree::Made) && !runner.make_for(&pending) {
                return (runner.notify)();
            }
            // Blocks until the login shell has said where programs are, a few seconds at most
            // and only once per app.
            let search_path = runner.agents.wait();
            if runner.slots().let_go(&id, generation) {
                return;
            }
            let launch = Launch {
                command: &pending.agent.command,
                cwd: &pending.thread.path,
                search_path: &search_path,
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
            let opened = starting.open_session();

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
                Ok(agent) => {
                    let root = &pending.thread.path;
                    begin(&agent, &pending.run, &pending.ids, &pending.prompt, root);
                    slot.agent = Held::Ready(pending.agent, agent);
                }
            }
            drop(slots);
            (runner.notify)();
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
            // A new thread's run is a run of notes, which takes snapshots: the one before is
            // the commit the worktree was made at.
            (Worktree::Make, _) => Some(pending.thread.fork.clone()),
            (Worktree::Remake { snapshot }, tip) => tip.filter(|_| *snapshot),
            (Worktree::Made, _) => None,
        };
        if let Some(before) = before {
            pending.run.record_snapshot(Point::Before, before);
        }
        true
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

    /// Writes a new thread for run `run`'s record, starting at commit `fork` and named from
    /// `title`.
    fn new_thread(&self, run: &Run, fork: &str, title: &str) -> Result<Thread> {
        let id = run.state().header.id.clone();
        let thread = thread::plan(&self.repo, self.home.as_deref(), &id, fork, title)?;
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
    let (run, thread, released, answer, next) = {
        let mut slots = runner.slots();
        if slots.let_go(id, generation) {
            return;
        }
        let slot = slots.by_thread.get_mut(id).expect("let_go found the slot");
        let thread = Some(slot.thread.clone());
        let review = slot
            .run
            .as_ref()
            .and_then(|run| run.state().header.review.clone());
        // Answered before the request is logged, and logged with it at once: a page that read
        // the request alone would show the run waiting for the engineer.
        // From the run's header, which never changes: a Stop that ends the chain does not lift
        // it for the rest of the proposer's turn.
        let read_only = review.as_ref().is_some_and(|stage| !stage.judges());
        // The review stage this run is, while its review goes on.
        let stage = review.filter(|stage| {
            slot.chain
                .as_ref()
                .is_some_and(|chain| chain.stage == *stage)
        });
        let answer = match (&event, &slot.agent) {
            (Event::Permission(request), Held::Ready(_, agent)) => {
                auto_answer(agent, request, read_only)
            }
            _ => None,
        };
        // A stage that does not end well ends its review.
        if ends_turn && !ended_well && stage.is_some() {
            slot.chain = None;
        }
        let ends_agent = matches!(
            event,
            Event::Exited { .. } | Event::TurnEnded(TurnEnd::Failed(_))
        );
        let released = if ends_agent {
            slot.release()
        } else {
            Held::None
        };
        let run = slot.run.clone();
        // However a proposer's turn ends, what it changed is put back before the end is
        // logged, under the lock, so no run starts in the worktree first.
        // Only for a live run: a kept proposer's agent that exits after its run ended (the
        // review found nothing, and the agent stayed) must not reset what the engineer has
        // committed or edited in the worktree since.
        let undone = run
            .as_deref()
            .filter(|run| ends_turn && run.state().live())
            .and_then(|run| undo_proposer(&runner, run, thread.as_ref()));
        // Decided under the lock, before the stage's end is logged: a reader that sees the end
        // sees the decision, and no Stop or new review can replace the chain meanwhile. The
        // next stage starts once the end is logged.
        let next = match (&stage, &run) {
            (Some(stage), Some(run)) if ended_well => {
                decide(slot, run, stage, undone).then(|| Arc::clone(run))
            }
            (_, Some(run)) => {
                // No findings are logged for a turn that did not end well: the run says it.
                if let Some(undone) = undone {
                    run.state().problem.get_or_insert(undone);
                }
                None
            }
            _ => None,
        };
        (run, thread, released, answer, next)
    };
    if let Some(run) = run {
        // The agent names files in the worktree it works in.
        let root = thread
            .as_ref()
            .map_or(runner.repo.root(), |thread| thread.path.as_path());
        // Before the end is logged, so a reader that sees the end sees what the run changed,
        // and its report.
        if ends_turn {
            runner.record_end_snapshot(&run, thread.as_ref());
        }
        let report = if ended_well {
            report_entry(&run, root)
        } else {
            None
        };
        if let Some(ids) = run.record_event(event, report, answer, root) {
            runner.mark_sent(&run, &ids);
        }
    }
    // Dropped outside the lock: dropping an agent waits for its process to be reaped.
    drop(released);
    (runner.notify)();
    if let Some(ended) = next {
        #[cfg(test)]
        runner.pass_next_stage_gate();
        runner.advance(id, &ended);
    }
}

/// Ends review stage `stage` of `run`, whose turn ended well in `thread`'s worktree, and decides
/// whether the review goes on: true when the next stage is to start, else false, the chain
/// then cleared. A proposer's findings are logged here, before the end, while the run still
/// takes entries.
fn decide(slot: &mut Slot, run: &Run, stage: &Stage, undone: Option<String>) -> bool {
    let found = slot.chain.as_ref().map_or(0, |chain| chain.findings.len());
    let findings = end_stage(run, stage, found, undone);
    let goes = match (findings, slot.chain.as_mut()) {
        (Some(findings), Some(chain)) => goes_on(chain, findings),
        _ => false,
    };
    if !goes {
        slot.chain = None;
    }
    goes
}

/// Answers a permission request as Postil does on the engineer's behalf (docs/design.md,
/// "Runs", decided with the owner on 2026-11-23: only a question stops for the engineer), and
/// returns the entry that logs the answer. A proposing reviewer is kept read-only: only its
/// reads, searches, fetches and thinking are allowed. `None` when no option fits, or when a
/// Stop already answered the request (and logged that) or the agent is gone; the run then
/// shows the request open until it ends.
fn auto_answer(agent: &Agent, request: &PermissionRequest, read_only: bool) -> Option<Entry> {
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
/// proposer's findings are read from its reply (numbered after the `found` before it) and
/// logged. `undone` says why the proposer's edits were put back ([`undo_proposer`]), if it
/// made any. Returns the findings for the next stage; `None` when the review ends here: this
/// was the judge, or a proposer changed files.
fn end_stage(
    run: &Run,
    stage: &Stage,
    found: usize,
    undone: Option<String>,
) -> Option<Vec<Finding>> {
    if stage.judges() {
        return None;
    }
    let reply = run.state().reply();
    let (findings, problem) = match review::findings_in(&reply, found + 1) {
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

/// [`undo_proposer`] for a run a forced Stop or a close ends, whose end the agent does not
/// report: only while it is live, and the run says what was undone.
fn undo_if_live(runner: &Runner, run: &Run, thread: Option<&Thread>) {
    if !run.state().live() {
        return;
    }
    if let Some(undone) = undo_proposer(runner, run, thread) {
        run.state().problem.get_or_insert(undone);
    }
}

/// When `run` is a proposing reviewer's and changed `thread`'s worktree, puts the worktree
/// back to the thread's branch at its tip ([`Repo::reset_to`], ignored files kept) and returns
/// what the run says about it; `None` for any other run, or a proposer that changed nothing.
/// Called however the stage's turn ends, a Stop and a close included: edits left there would
/// be taken in by the next run's commit, and by Apply with it.
///
/// A proposer may only read, but an agent that edits without asking is not stopped by the
/// permission rule. An edit made through a command reports no diff, one it committed leaves
/// no change, and one on another branch leaves `HEAD` anywhere, so the worktree is asked for
/// all three: a change, another branch checked out, or the branch moved. A worktree that
/// cannot be asked counts as changed. Runs git in the worktree, opened by [`thread::open`]:
/// `status`, `branch`, `rev-parse`, and the reset when needed.
fn undo_proposer(runner: &Runner, run: &Run, thread: Option<&Thread>) -> Option<String> {
    let reviewer = {
        let state = run.state();
        state
            .header
            .review
            .as_ref()
            .filter(|stage| !stage.judges())?
            .reviewer
    };
    let thread = thread?;
    let home = runner.home.as_deref()?;
    let said = |undone: String| {
        Some(format!(
            "reviewer {reviewer} changed files, though it may only read: {undone}, and the \
             review stops here"
        ))
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
    if !moved && run.state().work().diffs.is_empty() {
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

/// The prompt of a follow-up that goes to a fresh agent: the thread so far, each run's prompt
/// and reply in order, then the engineer's `text`, then the ask for a report ([`report_ask`]).
/// The diffs stay out: the agent reads the files as they are now, linked after the text.
fn fresh_follow_up(thread: &[Arc<Run>], text: &str) -> String {
    let mut prompt = String::from(
        "You are continuing work another coding agent did in this repository; its \
         conversation is gone. Here is what it was asked and what it answered, oldest first. \
         Read the files for their current state.\n",
    );
    for run in thread {
        let state = run.state();
        let asked = state
            .header
            .follow_up
            .as_ref()
            .map_or(state.header.prompt.as_str(), |follow_up| &follow_up.text);
        write!(
            prompt,
            "\n{} was asked:\n\n{}\n\nIt answered:\n\n{}\n",
            state.header.id,
            asked.trim_end(),
            state.reply().trim()
        )
        .expect("writing to a String cannot fail");
    }
    write!(
        prompt,
        "\nThe engineer now asks:\n\n{text}\n\n{}",
        report_ask(&thread[0].state().header.prompt)
    )
    .expect("writing to a String cannot fail");
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
/// reply in `root` (`report::read`); `None` for a review stage, which logs findings instead.
/// The caller logs it with the run's end, under one hold of the run: a forced stop or a close
/// that ends the run first takes neither. Asks the file system whether each step's path
/// exists, so it is never called under the `Slots` lock.
fn report_entry(run: &Run, root: &Path) -> Option<Entry> {
    let reply = {
        let state = run.state();
        // A run that a forced stop or a close ended takes no entry: no file is read for it.
        if state.header.review.is_some() || !state.live() {
            return None;
        }
        state.reply()
    };
    Some(match report::read(&reply, root) {
        Ok(report) => Entry::Report { report },
        Err(problem) => Entry::NoReport { problem },
    })
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
            }],
        };
        let shown = format!("{chain:?}");
        assert!(
            shown.contains("reviewer: 2") && shown.contains("findings: 1"),
            "{shown}"
        );
        assert!(!shown.contains("secret"), "{shown}");
    }
}
