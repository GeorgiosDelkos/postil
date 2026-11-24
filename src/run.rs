//! Runs: notes sent to the agent as one prompt, and everything the agent reports back.
//!
//! A [`Runner`] belongs to the open project. Each send starts a thread, which works in a
//! worktree and on a branch of its own (`thread.rs`, docs/design.md "Worktrees"). The runner
//! keeps the agent it started alive between the runs of its thread, with its session, so a
//! follow-up reaches the same conversation; the owner chose this over a fresh agent per run
//! on 2026-11-23. The agent is replaced when a run names another agent, a send starts another
//! thread, or it exited, and stopped when its thread ends, the project closes or the app
//! quits.
//!
//! One run at a time per repository, for now: a live run holds an exclusive lock on
//! `runs/active.lock`, so another window cannot start one either. Ending or forgetting a
//! thread takes the same lock, so no window's agent works in a worktree being freed.
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
//! taken in this order: the runner's [`Slot`], the repository's snapshot lock (inside
//! [`Repo::snapshot`], which takes no other), a run's state, then the agent connection's
//! own locks (inside [`Agent`]'s methods, which take none of the others). The record's
//! `threads.lock` (inside `thread.rs`, which takes no other) may be taken while the slot is
//! held, so nothing that holds it may take the slot. No snapshot is taken under a run's
//! state. The slot is held around the agent's calls that answer, cancel
//! and prompt (`auto_answer`, [`Runner::cancel`], `begin`). The page is told about every
//! change through `notify`, and fetches what it has not seen.

mod log;

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
use crate::review::{self, Diff, Finding, Stage, Work};
use crate::run::log::{
    Changes, Entry, FollowUp, Header, Origin, Point, Run, RunView, hold_run_lock, read_header,
    run_number, run_numbers,
};
use crate::thread::{self, State, Thread};

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
    slot: Mutex<Slot>,
    /// Holds a review's next stage, once, until a test lets it start
    /// ([`Runner::hold_next_stage`]): between a stage's end and the next stage's start is a
    /// window no request can otherwise be timed into.
    #[cfg(test)]
    next_stage_gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

/// What a runner holds now.
#[derive(Debug, Default)]
struct Slot {
    agent: Held,
    /// Counts the agents this runner let go. An agent's events and its start carry the number
    /// it was started with: when it differs, the agent was replaced, stopped, or its run
    /// cancelled, and what it reports is dropped.
    generation: u64,
    /// This window's latest run.
    run: Option<Arc<Run>>,
    /// The thread `run` works in, set with it at each launch: the agent's working directory
    /// is its worktree, and a kept agent is used again only within it.
    thread: Option<Thread>,
    /// The thread whose worktree the start thread is making (`git worktree add`, the clones),
    /// from the launch until it is made or fails. Git writes the worktree meanwhile, so a
    /// commit, a run or an ending there could act on a half-made checkout; the slot counts as
    /// busy. Making goes on past a Stop: it cannot be cut short.
    making: Option<String>,
    /// Set when the project closed: an agent that finishes starting afterwards is stopped.
    closed: bool,
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

impl Slot {
    /// Lets the agent go: a starting one is stopped here, under the caller's lock (that waits
    /// for `/bin/kill`, milliseconds), and anything either would still report is dropped. The
    /// caller drops the returned agent after releasing the slot's lock, which stops a ready
    /// one.
    #[must_use = "drop the released agent after the slot's lock: dropping it waits for its process"]
    fn release(&mut self) -> Held {
        if let Held::Starting(stopper) = &self.agent {
            stopper.stop();
        }
        self.generation += 1;
        std::mem::take(&mut self.agent)
    }

    /// Whether an agent started as number `generation` was let go meanwhile.
    fn let_go(&self, generation: u64) -> bool {
        self.closed || self.generation != generation
    }

    /// [`Runner::busy`]: a run under way, a review between two stages, or a worktree being
    /// made.
    fn busy(&self) -> bool {
        if self.making.is_some() {
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
#[derive(Debug, Clone, Copy)]
enum Worktree {
    Make,
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
    /// An open thread whose worktree is on disk.
    Open(Thread),
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
    ) -> Arc<Self> {
        if let Some(home) = &home {
            // Finishes deleting the worktrees of threads that ended while the app last ran:
            // a quit can cut a background delete short.
            thread::empty_trash(&thread::trash_for(repo.root(), home));
        }
        Arc::new(Self {
            repo,
            agents,
            notify,
            home,
            slot: Mutex::new(Slot::default()),
            #[cfg(test)]
            next_stage_gate: Mutex::new(None),
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
    /// starts there. Returns at once, except while another window's run holds the run lock:
    /// then it waits up to `LOCK_PATIENCE` under the runner's lock before it refuses. The run
    /// reports its progress through its log.
    ///
    /// # Errors
    ///
    /// [`Error::TurnRunning`] while a run of this window is under way, and
    /// [`Error::RunElsewhere`] while another window's is; as [`notes::prompt`];
    /// [`Error::NoCommit`] in a repository with no commit; [`Error::WriteRun`] when the run
    /// log cannot be created; and the errors of [`thread::plan`] and [`thread::add`], which
    /// fail the run.
    pub(crate) fn send(self: &Arc<Self>, agent: AgentChoice, ids: &[String]) -> Result<()> {
        let slot = self.slot();
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
        self.launch(slot, new)
    }

    /// The open thread `id`, whose worktree is on disk.
    ///
    /// # Errors
    ///
    /// [`Error::BadRequest`] when it has no thread (it ran before threads had worktrees), the
    /// thread ended, or its worktree is gone; as [`thread::find`].
    fn open_thread(&self, id: &str) -> Result<Thread> {
        let thread = thread::find(&self.repo, id)?.ok_or_else(|| {
            Error::BadRequest(format!(
                "{id} ran before each thread had a worktree of its own: send its notes again"
            ))
        })?;
        let ended = match thread.state {
            State::Open if thread.path.is_dir() => return Ok(thread),
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
    /// run, and takes a snapshot and may wait for the run lock as [`Runner::send`] does.
    ///
    /// # Errors
    ///
    /// [`Error::TurnRunning`] while a run of this window is under way, and
    /// [`Error::RunElsewhere`] while another window's is; [`Error::BadRequest`] for a blank
    /// text, a run that is a review, or a run another run already follows; as
    /// [`Runner::view_of`] for `of` and the runs it follows, and [`Error::NoSuchRun`] when one
    /// follows a run that is not older; [`Error::ReadRun`] when the runs directory cannot be
    /// listed; and [`Error::WriteRun`] when the run log cannot be created.
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
        let open = self.open_thread(&first)?;
        let slot = self.slot();
        // The kept agent ran `followed`, the slot's run, so it works in this thread.
        let same_conversation = matches!(
            &slot.agent,
            Held::Ready(kept, live) if *kept == agent && live.is_alive()
        ) && slot
            .run
            .as_ref()
            .is_some_and(|run| Arc::ptr_eq(run, followed));
        let prompt = if same_conversation {
            Prompt {
                text: text.to_owned(),
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
            place: Place::Open(open),
        };
        self.launch(slot, new)
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
    /// [`Error::TurnRunning`] while a run of this window is under way, and
    /// [`Error::RunElsewhere`] while another window's is; [`Error::BadRequest`] with no
    /// reviewers, for a run that is itself a review, or for a follow-up; as
    /// [`Runner::view_of`] for the reviewed run; and [`Error::WriteRun`] when the first
    /// stage's log cannot be created.
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
        self.start_stage(self.slot(), chain)
    }

    /// The run `of`, this window's when it is that one, else read from its log.
    fn run_by_id(&self, of: &str) -> Result<Arc<Run>> {
        let number = run_number(of)
            .ok_or_else(|| Error::BadRequest(format!("run id {of:?}: expected R and a number")))?;
        let own = self.slot().run.clone();
        if let Some(run) = own.filter(|run| run.state().header.id == of) {
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
    fn start_stage(self: &Arc<Self>, slot: MutexGuard<'_, Slot>, chain: Chain) -> Result<()> {
        let open = self.open_thread(&chain.stage.of)?;
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
            review: Some(chain),
            place: Place::Open(open),
        };
        self.launch(slot, new)
    }

    /// Starts the review's next stage, once `goes_on` moved the chain to it as stage `ended`
    /// ended. Runs on the ended agent's reader thread, and replaces that agent there, as
    /// [`on_event`] does with an agent that exited: [`Agent`]'s drop kills and reaps the
    /// process but never joins its threads, and the reader holds none of the agent's locks
    /// while it delivers an event.
    fn advance(self: &Arc<Self>, ended: &Arc<Run>) {
        let mut slot = self.slot();
        // A send, a stop, or another review meanwhile took the chain, or replaced its run.
        if !slot.run.as_ref().is_some_and(|run| Arc::ptr_eq(run, ended)) {
            return;
        }
        let Some(chain) = slot.chain.take() else {
            return;
        };
        if let Err(error) = self.start_stage(slot, chain) {
            // The chain went with the stage that could not start, so the review ends, and the
            // stage that ended says why.
            ended.state().problem.get_or_insert(format!(
                "the next reviewer could not start: {}",
                error_chain(&error)
            ));
            (self.notify)();
        }
    }

    /// Makes `new` this window's run and its review the one under way, sends the run to its
    /// agent, then tells the page. Takes the snapshot before the run, except for a proposer,
    /// which may not edit: a new thread's is the commit it starts from, an open thread's the
    /// commit its worktree is at, after committing anything left there. The slot changes only
    /// once the run's log exists, so a launch that fails leaves the review and the kept agent
    /// as they were. The agent it replaces is dropped after `slot` is released.
    ///
    /// # Errors
    ///
    /// [`Error::TurnRunning`] while a run of this window is under way, as [`Run::create`], and
    /// for a new thread as [`thread::plan`] and [`thread::add`]: the run then fails too.
    fn launch(self: &Arc<Self>, mut slot: MutexGuard<'_, Slot>, new: NewRun) -> Result<()> {
        // A worktree still being made counts as a run under way: its agent has not started.
        if slot.making.is_some() || slot.run.as_ref().is_some_and(|run| run.state().live()) {
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
        let run = Arc::new(Run::create(
            &self.repo.runs_dir(),
            &agent.name,
            &notes,
            &prompt.text,
            origin,
        )?);
        let (thread, worktree) = match place {
            Place::New { fork, title } => match self.new_thread(&run, &fork, &title) {
                Ok(thread) => (thread, Worktree::Make),
                Err(error) => {
                    run.record(Entry::Failed {
                        error: error_chain(&error),
                    });
                    (self.notify)();
                    return Err(error);
                }
            },
            Place::Open(thread) => (thread, Worktree::Made),
        };

        slot.chain = review;
        if snapshot {
            match worktree {
                Worktree::Make => run.record_snapshot(Point::Before, thread.fork.clone()),
                Worktree::Made => self.record_snapshot(&run, &thread, Point::Before),
            }
        }
        if let Worktree::Make = worktree {
            slot.making = Some(thread.id.clone());
        }
        let same_thread = slot
            .thread
            .as_ref()
            .is_some_and(|kept| kept.id == thread.id);
        slot.run = Some(Arc::clone(&run));
        slot.thread = Some(thread.clone());
        let replaced = if let (Held::Ready(choice, kept), Conversation::Kept) =
            (&slot.agent, conversation)
            && same_thread
            && *choice == agent
            && kept.is_alive()
        {
            begin(kept, &run, &marks, &prompt, &thread.path);
            Held::None
        } else {
            // The agent kept from the last run, if any, is stopped when this is dropped.
            let replaced = slot.release();
            let pending = Pending {
                agent,
                run,
                ids: marks,
                prompt,
                generation: slot.generation,
                thread,
                worktree,
            };
            self.start(pending);
            replaced
        };
        drop(slot);
        // Dropping an agent waits for its process to be reaped.
        drop(replaced);
        (self.notify)();
        Ok(())
    }

    /// Stops the run (module docs, "Stopping").
    ///
    /// # Errors
    ///
    /// [`Error::BadRequest`] when no run is under way; [`Error::AgentExited`] when the agent
    /// is gone (its exit then ends the run).
    pub(crate) fn cancel(&self) -> Result<()> {
        let mut slot = self.slot();
        let run = slot
            .run
            .clone()
            .filter(|run| run.state().live())
            .ok_or_else(|| Error::BadRequest("no run is under way".to_owned()))?;
        let (started, asked) = {
            let state = run.state();
            (state.started, state.cancel_asked)
        };
        let thread = slot.thread.clone();
        let mut released = Held::None;
        let mut forced = false;
        let mut result = Ok(());
        // A stopped reviewer ends its review: the next stage does not start.
        slot.chain = None;
        match (started, asked) {
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
        drop(slot);
        // Dropping the agent kills it and waits for its process to be reaped.
        drop(released);
        if forced {
            // Its exit is not reported, so the snapshot after it is taken here, once nothing
            // can write any more. The run stays live until then, so a send is refused
            // (`TurnRunning`) meanwhile.
            self.record_end_snapshot(&run, thread.as_ref());
            run.record(Entry::Failed {
                error: "stopped: the agent did not stop when asked, so Postil ended it".to_owned(),
            });
        }
        (self.notify)();
        result
    }

    /// Answers the run's question `request` (its number, as the log gives it) with the form's
    /// values, or with `None` to skip it.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchRequest`] when no agent is running, and as [`Agent::answer_question`].
    pub(crate) fn reply(
        &self,
        request: usize,
        content: Option<serde_json::Map<String, Value>>,
    ) -> Result<()> {
        let slot = self.slot();
        let (Some(run), Held::Ready(_, agent)) = (&slot.run, &slot.agent) else {
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
        drop(slot);
        (self.notify)();
        Ok(())
    }

    /// Answers the run's permission request `request` (its number, as the log gives it) with
    /// the option `option`.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchRequest`] when no agent is running, and as
    /// [`Agent::answer_permission`], which keeps the table of open requests.
    pub(crate) fn answer(&self, request: usize, option: &str) -> Result<()> {
        let slot = self.slot();
        let (Some(run), Held::Ready(_, agent)) = (&slot.run, &slot.agent) else {
            return Err(Error::NoSuchRequest);
        };
        // The agent checks the option against what it offered; only an accepted answer is
        // logged, so a refused one can be made again.
        agent.answer_permission(request, option)?;
        run.record(Entry::Answer {
            request,
            option: Some(option.to_owned()),
        });
        drop(slot);
        (self.notify)();
        Ok(())
    }

    /// The latest run, with the entries from number `after` on, and its notes and prompt only
    /// when `after` is 0: this window's run, or the newest readable log on disk when this
    /// window has run nothing yet. `None` when there is no run.
    ///
    /// # Errors
    ///
    /// [`Error::ReadRun`] when the runs directory cannot be listed.
    pub(crate) fn view(&self, after: usize) -> Result<Option<RunView>> {
        let run = self.slot().run.clone();
        match run {
            Some(run) => Ok(Some(run.view(after))),
            None => Ok(Run::latest(&self.repo.runs_dir())?.map(|run| run.view(after))),
        }
    }

    /// The run `id` (`R2`), with all its entries: this window's run when it is that one, else
    /// the run as its log on disk has it.
    ///
    /// # Errors
    ///
    /// [`Error::BadRequest`] when `id` is not of the form `R<number>`; [`Error::NoSuchRun`]
    /// when its log is missing or cannot be read.
    pub(crate) fn view_of(&self, id: &str) -> Result<RunView> {
        Ok(self.run_by_id(id)?.view(0))
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
    /// worktrees.
    ///
    /// # Errors
    ///
    /// [`Error::BadRequest`] when the thread's worktree is gone, and as [`Runner::thread_of`]
    /// and [`Repo::snapshot`].
    fn files_now(&self, run: &Run) -> Result<String> {
        match self.thread_of(run)? {
            None => self.repo.snapshot(),
            Some(thread) if thread.state == State::Open && thread.path.is_dir() => {
                Repo::open(&thread.path)?.snapshot()
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

    /// The thread `id` as its record has it, or `None` when there is none (a run from before
    /// threads had worktrees).
    ///
    /// # Errors
    ///
    /// As [`thread::find`].
    pub(crate) fn thread_record(&self, id: &str) -> Result<Option<Thread>> {
        thread::find(&self.repo, id)
    }

    /// Ends the open thread `id` as `ending` says (docs/design.md, "Ending a thread"), and
    /// frees its worktree: renamed into the trash and deleted in the background. Its branch
    /// stays, until the thread's last note is deleted.
    ///
    /// Holds the run lock throughout, so no run, in this window or another, works in the
    /// worktree meanwhile, and reads the thread only once it holds it. First checks that the
    /// worktree holds nothing Postil did not commit, and that its branch is at the commit
    /// Postil left or past it (a commit the engineer made there). Apply writes the branch's
    /// changes, from the commit the thread started at to the worktree's commit, into the
    /// engineer's working tree, uncommitted, all or nothing. The thread's kept agent is stopped
    /// once Apply succeeded. The thread is marked ended only once its worktree is freed, so a
    /// free that fails leaves it open, to end again.
    ///
    /// # Errors
    ///
    /// [`Error::NoHome`] without Postil's home; [`Error::TurnRunning`] while one of its runs
    /// is under way in this window, its review is between two stages, or its worktree is
    /// being made, and [`Error::RunElsewhere`] while another window runs one;
    /// [`Error::BadRequest`] when there is no open thread `id`; [`Error::ThreadChanged`] when
    /// the check fails; [`Error::ApplyConflict`] when the changes do not fit the working tree,
    /// which then stays as it was; and as [`thread::free`]: an applied thread whose worktree
    /// cannot be freed stays open with its changes written.
    pub(crate) fn end_thread(&self, id: &str, ending: Ending) -> Result<()> {
        let home = self.home.as_deref().ok_or(Error::NoHome)?;
        {
            let slot = self.slot();
            if slot.thread.as_ref().is_some_and(|held| held.id == id) && slot.busy() {
                return Err(Error::TurnRunning);
            }
        }
        let _active = hold_run_lock(&self.repo.runs_dir())?;
        let thread = match thread::find(&self.repo, id)? {
            Some(thread) if thread.state == State::Open => thread,
            Some(_) => return Err(Error::BadRequest(format!("thread {id} already ended"))),
            None => return Err(Error::BadRequest(format!("there is no thread {id}"))),
        };
        thread::check_place(&self.repo, home, &thread)?;
        let tip = check_worktree(&thread)?;
        if ending == Ending::Apply {
            let patch = self.repo.patch(&thread.fork, &tip)?;
            self.repo.apply(&patch).map_err(|error| match error {
                Error::Git { stderr, .. } => Error::ApplyConflict { stderr },
                error => error,
            })?;
        }
        let released = {
            let mut slot = self.slot();
            if slot.thread.as_ref().is_some_and(|held| held.id == id) {
                slot.release()
            } else {
                Held::None
            }
        };
        // Dropping the agent waits for its process, so after the lock.
        drop(released);
        let ended = thread::free(&self.repo, home, &thread).and_then(|()| {
            thread::update(&self.repo, id, |stored| {
                stored.tip = tip;
                stored.state = match ending {
                    Ending::Apply => State::Applied,
                    Ending::Discard => State::Discarded,
                };
            })
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
    /// # Errors
    ///
    /// [`Error::ReadRun`] when the runs directory cannot be listed, as [`notes::ids`] for the
    /// review file, [`Error::WriteRun`] when a log cannot be removed. Some logs may be removed
    /// by then; the rest are tried again at the next delete.
    pub(crate) fn delete_runs_without_notes(&self) -> Result<()> {
        let dir = self.repo.runs_dir();
        let numbers = run_numbers(&dir).map_err(|source| Error::ReadRun {
            path: dir.clone(),
            source,
        })?;
        let left = notes::ids(&self.repo)?;
        // Read once: the slot is not held while logs are read, so the agent's events, which
        // need it, are never kept waiting on the disk.
        let live = {
            let slot = self.slot();
            slot.run
                .as_ref()
                .filter(|run| run.state().live())
                .map(|run| run.state().header.id.clone())
        };
        let mut deleted = Vec::new();
        for number in numbers {
            let id = format!("R{number}");
            if live.as_deref() == Some(id.as_str()) {
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
        let live_thread = {
            let mut slot = self.slot();
            if slot
                .run
                .as_ref()
                .is_some_and(|run| deleted.contains(&run.state().header.id))
            {
                slot.run = None;
            }
            // Busy, not only live: a review between stages starts its next one in the thread.
            let busy = slot.busy();
            slot.thread
                .as_ref()
                .filter(|_| busy)
                .map(|thread| thread.id.clone())
        };
        let forgotten = self.forget_threads(live_thread.as_deref());
        (self.notify)();
        forgotten
    }

    /// Forgets every thread whose first run's log is gone, except `busy`, the thread this
    /// window is working in: frees its worktree, deletes its branch, and removes it from the
    /// record. Stops a kept agent that works in one. Holds the run lock, so no window's run
    /// works in a worktree it frees; while a run holds it, in any window, it forgets nothing,
    /// and the next delete tries again. A follow-up still running when its thread's first log
    /// went keeps the thread until a delete after it ends.
    ///
    /// The worktree must pass the check that ending a thread makes: work Postil did not
    /// commit is not deleted with a note. Only a branch under `postil/` is deleted, since the
    /// name comes from the record, which a hand edit can change.
    ///
    /// # Errors
    ///
    /// As [`thread::all`], [`check_worktree`], [`thread::free`], [`Repo::delete_branch`] and
    /// [`thread::remove`]. A thread that fails one of them stays in the record, and is tried
    /// again at the next delete.
    fn forget_threads(&self, busy: Option<&str>) -> Result<()> {
        let Some(home) = self.home.as_deref() else {
            return Ok(());
        };
        let dir = self.repo.runs_dir();
        let _active = match hold_run_lock(&dir) {
            Ok(active) => active,
            Err(Error::RunElsewhere) => return Ok(()),
            Err(error) => return Err(error),
        };
        let gone: Vec<Thread> = thread::all(&self.repo)?
            .into_iter()
            .filter(|thread| Some(thread.id.as_str()) != busy)
            .filter(|thread| !dir.join(format!("{}.jsonl", thread.id)).exists())
            .collect();
        if gone.is_empty() {
            return Ok(());
        }
        let released = {
            let mut slot = self.slot();
            let kept = slot.thread.as_ref().map(|thread| thread.id.clone());
            if kept.is_some_and(|kept| gone.iter().any(|thread| thread.id == kept)) {
                slot.thread = None;
                slot.release()
            } else {
                Held::None
            }
        };
        // Dropping the agent waits for its process, so after the lock.
        drop(released);
        let mut first_error = None;
        let mut removed = Vec::new();
        for thread in gone {
            // In any state: an ended thread whose free failed still has its worktree, which
            // keeps the branch checked out.
            let forgotten = thread::check_place(&self.repo, home, &thread)
                .and_then(|()| check_worktree(&thread))
                .and_then(|_| thread::free(&self.repo, home, &thread))
                .and_then(|()| {
                    if thread.branch.starts_with("postil/") {
                        self.repo.delete_branch(&thread.branch)
                    } else {
                        Ok(())
                    }
                });
            match forgotten {
                Ok(()) => removed.push(thread.id),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        thread::remove(&self.repo, &removed)?;
        first_error.map_or(Ok(()), Err)
    }

    /// Whether this window has a run under way, or a review between two stages, whose next
    /// stage starts with no request from the page.
    pub(crate) fn busy(&self) -> bool {
        self.slot().busy()
    }

    /// Stops the agent, running or starting, and ignores anything it would still report: the
    /// project is closing, or the app quits. A run under way ends as failed, saying so, so it
    /// does not read as another window's.
    pub(crate) fn close(&self) {
        let (released, run, thread) = {
            let mut slot = self.slot();
            slot.closed = true;
            slot.chain = None;
            // A worktree git is still checking out takes no commit: it would commit a half
            // checkout (every file not yet written, as deleted).
            let thread = slot.thread.clone().filter(|_| slot.making.is_none());
            (slot.release(), slot.run.clone(), thread)
        };
        // The agent is gone first, so nothing it reports lands after the end. `closed` drops
        // its events anyway.
        drop(released);
        if let Some(run) = run {
            // As a forced Stop does: the agent's exit is not reported, so the snapshot after
            // the run is taken here, or its changes view would stay empty. It runs `git add`
            // on the request thread, quitting included. Both calls do nothing for a run that
            // already ended, or one whose worktree is still being made.
            self.record_end_snapshot(&run, thread.as_ref());
            run.record(Entry::Failed {
                error: "stopped: Postil closed the project while it ran".to_owned(),
            });
        }
    }

    fn slot(&self) -> MutexGuard<'_, Slot> {
        lock(&self.slot)
    }

    /// Starts the agent on its own thread, then sends the prompt, unless the agent was let go
    /// meanwhile (the run cancelled, the project closed). A new thread's worktree is made
    /// there first ([`thread::make`]), which takes as long as a checkout.
    fn start(self: &Arc<Self>, pending: Pending) {
        let runner = Arc::clone(self);
        let run = Arc::clone(&pending.run);
        let start = move || {
            let generation = pending.generation;
            let failed = |error: &Error| {
                pending.run.record(Entry::Failed {
                    error: error_chain(error),
                });
            };
            if let Worktree::Make = pending.worktree {
                let made = runner.make_worktree(&pending.thread);
                let mut slot = runner.slot();
                slot.making = None;
                if let Err(error) = made {
                    if !slot.let_go(generation) {
                        failed(&error);
                    }
                    drop(slot);
                    return (runner.notify)();
                }
            }
            // Blocks until the login shell has said where programs are, a few seconds at most
            // and only once per app.
            let search_path = runner.agents.wait();
            if runner.slot().let_go(generation) {
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
            let spawned = Agent::spawn(&launch, move |event| on_event(&events, generation, event));
            let starting = match spawned {
                Ok(starting) => starting,
                Err(error) => {
                    if !runner.slot().let_go(generation) {
                        failed(&error);
                    }
                    return (runner.notify)();
                }
            };

            // Stoppable from here on; a cancel or close that came first wins, and dropping
            // `starting` stops it.
            {
                let mut slot = runner.slot();
                if slot.let_go(generation) {
                    return;
                }
                slot.agent = Held::Starting(starting.stopper());
            }
            let opened = starting.open_session();

            let mut slot = runner.slot();
            if slot.let_go(generation) {
                // The run already says why. An agent that started after all is stopped by
                // dropping it, after the lock: dropping it waits for its process.
                drop(slot);
                drop(opened);
                return;
            }
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
            drop(slot);
            (runner.notify)();
        };
        let spawned = std::thread::Builder::new()
            .name("agent-start".to_owned())
            .spawn(start);
        if let Err(error) = spawned {
            run.record(Entry::Failed {
                error: format!("cannot start a thread for the agent: {error}"),
            });
        }
    }

    /// Makes `thread`'s worktree ([`thread::make`]), and records in the thread what it was not
    /// given. A record that cannot be written loses that list only.
    ///
    /// A worktree that cannot be made takes the thread out of the record: it claims a branch
    /// that `git worktree add` may not have made, which a thread of another working tree of
    /// the repository may own, and deleting the thread's last note would delete it.
    fn make_worktree(&self, thread: &Thread) -> Result<()> {
        let skipped = match thread::make(&self.repo, thread) {
            Ok(skipped) => skipped,
            Err(error) => {
                // The run says why; a record that cannot be changed stays, as it was.
                drop(thread::remove(&self.repo, std::slice::from_ref(&thread.id)));
                return Err(error);
            }
        };
        if !skipped.is_empty() {
            drop(thread::update(&self.repo, &thread.id, |stored| {
                stored.skipped = skipped;
            }));
        }
        Ok(())
    }

    /// Records the snapshot at `point` in `run`: the commit `thread`'s worktree is at, once
    /// everything in it is committed ([`Repo::commit_all`]), so a snapshot is a commit on the
    /// thread's branch. After a run, the commit becomes the thread's tip. One that cannot be
    /// taken is the run's problem: the run goes on, without its changes view, and the work
    /// stays uncommitted in the worktree.
    fn record_snapshot(&self, run: &Run, thread: &Thread, point: Point) {
        let message = commit_message(&run.state().header, point);
        let committed = Repo::open(&thread.path).and_then(|tree| tree.commit_all(&message));
        // Before a run too: a commit of work left in the worktree moves the branch, and a run
        // that then never starts takes no snapshot after it.
        let commit = committed.and_then(|commit| {
            thread::update(&self.repo, &thread.id, |stored| {
                stored.tip.clone_from(&commit);
            })?;
            Ok(commit)
        });
        match commit {
            Ok(commit) => run.record_snapshot(point, commit),
            Err(error) => {
                run.state().problem.get_or_insert(format!(
                    "no snapshot of the files was taken: {}",
                    error_chain(&error)
                ));
            }
        }
    }

    /// Records the snapshot after `run`, in `thread`, as its turn ends: only while it is live,
    /// has the one before and not yet one after, so an agent that exits after its run ended
    /// adds none.
    fn record_end_snapshot(&self, run: &Run, thread: Option<&Thread>) {
        let wanted = {
            let state = run.state();
            state.live()
                && state.snapshot(Point::Before).is_some()
                && state.snapshot(Point::After).is_none()
        };
        if let Some(thread) = thread.filter(|_| wanted) {
            self.record_snapshot(run, thread, Point::After);
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
        self.slot().chain.is_some()
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

    /// Moves the run's first Stop back by [`FORCE_AFTER`], so a test can force a stop without
    /// waiting that long.
    pub(crate) fn backdate_stop(&self) {
        if let Some(run) = &self.slot().run {
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
            .field("slot", &*self.slot())
            .finish_non_exhaustive()
    }
}

/// An agent's event, on its reader thread: recorded in the current run unless the agent
/// was let go. An agent that exited, or whose turn failed, is let go: the next send starts a
/// fresh one, since a failed turn can mean a broken session (an adapter whose backend died,
/// a sign-in that expired), and a lost conversation costs less than a stuck one.
fn on_event(runner: &Weak<Runner>, generation: u64, event: Event) {
    let Some(runner) = runner.upgrade() else {
        return;
    };
    let ended_well = matches!(
        event,
        Event::TurnEnded(TurnEnd::Stopped(StopReason::EndTurn))
    );
    let ends_turn = matches!(event, Event::TurnEnded(_) | Event::Exited { .. });
    let (run, thread, released, answer, next) = {
        let mut slot = runner.slot();
        if slot.let_go(generation) {
            return;
        }
        let thread = slot.thread.clone();
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
        // Decided under the lock, before the stage's end is logged: a reader that sees the end
        // sees the decision, and no Stop or new review can replace the chain meanwhile. The
        // next stage starts once the end is logged.
        let next = match (&stage, &run) {
            (Some(stage), Some(run)) if ended_well => {
                decide(&mut slot, run, stage, thread.as_ref()).then(|| Arc::clone(run))
            }
            _ => None,
        };
        (run, thread, released, answer, next)
    };
    if let Some(run) = run {
        // Before the end is logged, so a reader that sees the end sees what the run changed.
        if ends_turn {
            runner.record_end_snapshot(&run, thread.as_ref());
        }
        // The agent names files in the worktree it works in.
        let root = thread
            .as_ref()
            .map_or(runner.repo.root(), |thread| thread.path.as_path());
        if let Some(ids) = run.record_event(event, answer, root) {
            runner.mark_sent(&run, &ids);
        }
    }
    // Dropped outside the lock: dropping an agent waits for its process to be reaped.
    drop(released);
    (runner.notify)();
    if let Some(ended) = next {
        #[cfg(test)]
        runner.pass_next_stage_gate();
        runner.advance(&ended);
    }
}

/// Ends review stage `stage` of `run`, whose turn ended well in `thread`'s worktree, and decides
/// whether the review goes on: true when the next stage is to start, else false, the chain
/// then cleared. A proposer's findings are logged here, before the end, while the run still
/// takes entries.
fn decide(slot: &mut Slot, run: &Run, stage: &Stage, thread: Option<&Thread>) -> bool {
    let found = slot.chain.as_ref().map_or(0, |chain| chain.findings.len());
    let findings = end_stage(run, stage, found, thread);
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

/// Ends review stage `stage` of `run`, whose turn ended well in `thread`'s worktree, before
/// its end is logged: a proposer's findings are read from its reply (numbered after the
/// `found` before it) and logged. Returns them for the next stage; `None` when the review ends
/// here: this was the judge, or a proposer changed files, which are then put back.
fn end_stage(
    run: &Run,
    stage: &Stage,
    found: usize,
    thread: Option<&Thread>,
) -> Option<Vec<Finding>> {
    if stage.judges() {
        return None;
    }
    let reply = run.state().reply();
    let (findings, problem) = match review::findings_in(&reply, found + 1) {
        Ok(findings) => (findings, None),
        Err(problem) => (Vec::new(), Some(problem)),
    };
    // A proposer may only read, but an agent that edits without asking is not stopped by the
    // permission rule: its edits are checked here, and they end the review, so no judge takes
    // a changed tree for the reviewed one. An edit made through a command reports no diff, and
    // one it committed leaves no change, so the worktree is asked for both; one that cannot
    // be asked counts as changed.
    let moved = |thread: &Thread| {
        Repo::open(&thread.path)
            .and_then(|tree| {
                Ok(!tree.changed()?.is_empty() || tree.head()? != Some(thread.tip.clone()))
            })
            .unwrap_or(true)
    };
    let edited = !run.state().work().diffs.is_empty() || thread.is_some_and(moved);
    let problem = if edited {
        // Put back, or the next run's commit would take them in, and Apply with it.
        let undone = thread.map_or(Ok(()), |thread| {
            Repo::open(&thread.path).and_then(|tree| tree.reset_to(&thread.tip))
        });
        let undone = match undone {
            Ok(()) => "its changes were undone".to_owned(),
            Err(error) => format!("its changes could not be undone: {}", error_chain(&error)),
        };
        Some(format!(
            "reviewer {} changed files, though it may only read: {undone}, and the review \
             stops here",
            stage.reviewer
        ))
    } else {
        problem
    };
    run.record(Entry::Findings {
        findings: findings.clone(),
        problem,
    });
    (!edited).then_some(findings)
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
/// and reply in order, then the engineer's `text`. The diffs stay out: the agent reads the
/// files as they are now, linked after the text.
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
    write!(prompt, "\nThe engineer now asks:\n\n{text}\n")
        .expect("writing to a String cannot fail");
    prompt
}

/// Checks that `thread`'s worktree holds nothing that ending the thread would lose: no change
/// git would commit (ignored files, such as the cloned build folders, do not count), and its
/// branch at the commit Postil left or past it. Past it is a commit the engineer made there,
/// as the refusal tells them to, and it is theirs to keep. Returns the commit the thread ends
/// at: the worktree's, or the recorded tip when the worktree is not on disk.
///
/// # Errors
///
/// [`Error::ThreadChanged`] naming what is there, and the git errors.
fn check_worktree(thread: &Thread) -> Result<String> {
    if !thread.path.is_dir() {
        return Ok(thread.tip.clone());
    }
    let tree = Repo::open(&thread.path)?;
    let changed = tree.changed()?;
    if !changed.is_empty() {
        return Err(Error::ThreadChanged {
            path: thread.path.clone(),
            found: format!("changed files {}", changed.join(", ")),
        });
    }
    match tree.head()? {
        Some(head) if tree.is_ancestor(&thread.tip, &head)? => Ok(head),
        head => Err(Error::ThreadChanged {
            path: thread.path.clone(),
            found: format!(
                "the branch is at {}, which does not follow {}, where Postil left it",
                head.as_deref().unwrap_or("no commit"),
                thread.tip
            ),
        }),
    }
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
