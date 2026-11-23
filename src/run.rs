//! Runs: notes sent to the agent as one prompt, and everything the agent reports back.
//!
//! A [`Runner`] belongs to the open project. It keeps the agent it started alive between
//! runs, with its session, so a later prompt reaches the same conversation; the owner chose
//! this over a fresh agent per send on 2026-11-23, for milestone 4's follow-up notes. The
//! agent is replaced when a send names another agent or it exited, and stopped when the
//! project closes or the app quits.
//!
//! One run at a time per working tree: two agents writing one working tree conflict
//! (docs/design.md, "Not in scope"). A live run holds an exclusive lock on
//! `runs/active.lock`, so another window on the same working tree cannot start one either.
//!
//! # Stopping
//!
//! Stop asks the agent to cancel the turn (`session/cancel`). An agent that is still
//! starting is killed instead, and the run ends at once: a start can take minutes. Asked
//! again from [`FORCE_AFTER`] on, Stop kills an agent that did not stop, so a hung agent never
//! holds the working tree; a second Stop sooner is the same request.
//!
//! # The run log
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
//! the counter (`last_run`). A log with no end is shown as unfinished: the app quit or the
//! project was closed while it ran, or another window is running it.
//!
//! # Threads and locks
//!
//! Requests from the page run on the main thread, the agent's events on its reader thread,
//! an agent's start on a thread of its own, and a review's next stage on one more
//! (`review-next`, [`Runner::advance`]). Two locks, always taken in this order: the
//! runner's [`Slot`], then a run's state. The page is told about every change through
//! `notify`, and fetches what it has not seen.

use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, File, TryLockError};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::acp::{
    Agent, ContentBlock, ElicitationReply, Event, Launch, Location, PermissionKind,
    PermissionOption, PermissionRequest, PlanEntryStatus, SessionUpdate, StopReason, Stopper,
    ToolCallContent, ToolCallStatus, ToolKind, TurnEnd,
};
use crate::agents::Scanner;
use crate::error::{Error, Result, error_chain};
use crate::notes::{self, Prompt};
use crate::repo::Repo;
use crate::review::{self, Diff, Finding, Stage, Work};

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

/// How long a send waits for the run lock before it reports another window's run
/// ([`take_run_lock`]). Arbitrary: far longer than a fork-to-exec window, short enough that a
/// refused send answers at once.
const LOCK_PATIENCE: Duration = Duration::from_millis(200);

/// The run log's format version, in its header line. Version 2 added a reviewer's run
/// (`review` in the header, the `findings` entry); version 1 logs read as runs of notes.
const LOG_VERSION: u32 = 2;

/// Tells the page that a run changed. In the app it wakes the event loop, which calls the
/// page; tests count the calls.
pub(crate) type Notify = Arc<dyn Fn() + Send + Sync>;

/// The agent a run goes to: its name in the settings and the command that starts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentChoice {
    pub(crate) name: String,
    pub(crate) command: String,
}

/// The runs of the open project, and the agent kept between them.
pub(crate) struct Runner {
    repo: Arc<Repo>,
    agents: Scanner,
    notify: Notify,
    slot: Mutex<Slot>,
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
    /// Set when the project closed: an agent that finishes starting afterwards is stopped.
    closed: bool,
    /// The review under way, while its stages run one after another; cleared when a stage
    /// does not end well or reports edits it may not make, when the engineer stops it, by a
    /// send, when the project closes, and when the last stage ends; `advance` takes it to
    /// start the next stage.
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
}

/// A send that waits for its agent to start.
#[derive(Debug)]
struct Pending {
    agent: AgentChoice,
    run: Arc<Run>,
    ids: Vec<String>,
    prompt: Prompt,
    generation: u64,
}

impl Runner {
    pub(crate) fn new(repo: Arc<Repo>, agents: Scanner, notify: Notify) -> Arc<Self> {
        Arc::new(Self {
            repo,
            agents,
            notify,
            slot: Mutex::new(Slot::default()),
        })
    }

    /// The prompt the notes `ids` make, as the agent will receive its text.
    ///
    /// # Errors
    ///
    /// As [`notes::prompt`].
    pub(crate) fn preview(&self, ids: &[String]) -> Result<String> {
        Ok(notes::prompt(&self.repo, ids)?.text)
    }

    /// Starts a run: sends the notes `ids` to `agent`, starting the agent first unless it is
    /// the one kept from the last run and still alive. Returns at once, except while another
    /// window's run holds the run lock: then it waits up to [`LOCK_PATIENCE`] under the
    /// runner's lock before it refuses. The run reports its progress through its log.
    ///
    /// # Errors
    ///
    /// [`Error::TurnRunning`] while a run of this window is under way, and
    /// [`Error::RunElsewhere`] while another window's is; as [`notes::prompt`]; and
    /// [`Error::WriteRun`] when the run log cannot be created.
    pub(crate) fn send(self: &Arc<Self>, agent: AgentChoice, ids: &[String]) -> Result<()> {
        let mut slot = self.slot();
        if slot.run.as_ref().is_some_and(|run| run.state().live()) {
            return Err(Error::TurnRunning);
        }
        let prompt = notes::prompt(&self.repo, ids)?;
        slot.chain = None;
        // The agent kept from a reviewer's run holds that reviewer's conversation (told not to
        // change files, or to judge findings), so a send after a review starts afresh.
        let after_review = slot
            .run
            .as_ref()
            .is_some_and(|run| run.state().header.review.is_some());
        let run = Arc::new(Run::create(
            &self.repo.runs_dir(),
            &agent.name,
            ids,
            &prompt.text,
            None,
        )?);
        slot.run = Some(Arc::clone(&run));

        let replaced = match &slot.agent {
            Held::Ready(kept, live) if *kept == agent && live.is_alive() && !after_review => {
                self.begin(live, &run, ids, &prompt);
                Held::None
            }
            _ => {
                // The agent kept from the last run, if any, is stopped when this is dropped.
                let replaced = slot.release();
                let pending = Pending {
                    agent,
                    run,
                    ids: ids.to_vec(),
                    prompt,
                    generation: slot.generation,
                };
                self.start(pending);
                replaced
            }
        };
        drop(slot);
        drop(replaced);
        (self.notify)();
        Ok(())
    }

    /// Starts a review of run `of` by `reviewers`, in order (`review.rs`): each stage is a run
    /// of its own, started with a fresh agent when the one before it ends well.
    ///
    /// # Errors
    ///
    /// [`Error::TurnRunning`] while a run of this window is under way, and
    /// [`Error::RunElsewhere`] while another window's is; [`Error::BadRequest`] with no
    /// reviewers, or for a run that is itself a review; as [`Runner::view_of`] for the
    /// reviewed run; and [`Error::WriteRun`] when the first stage's log cannot be created.
    pub(crate) fn review(self: &Arc<Self>, of: &str, reviewers: Vec<AgentChoice>) -> Result<()> {
        if reviewers.is_empty() {
            return Err(Error::BadRequest(
                "no reviewers are set: add them in Settings".to_owned(),
            ));
        }
        let reviewed = self.run_by_id(of)?;
        let mut slot = self.slot();
        if slot.run.as_ref().is_some_and(|run| run.state().live()) {
            return Err(Error::TurnRunning);
        }
        let (notes, work, files) = {
            let state = reviewed.state();
            if state.header.review.is_some() {
                return Err(Error::BadRequest(format!("{of} is itself a review")));
            }
            let work = state.work();
            let mut files: Vec<String> = Vec::new();
            for diff in &work.diffs {
                if !files.contains(&diff.path) {
                    files.push(diff.path.clone());
                }
            }
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
        let replaced = self.start_stage(&mut slot, chain)?;
        drop(slot);
        drop(replaced);
        (self.notify)();
        Ok(())
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
    /// inherits the reviewed agent's conversation. Returns the agent it replaced, to drop
    /// outside the lock.
    fn start_stage(self: &Arc<Self>, slot: &mut Slot, chain: Chain) -> Result<Held> {
        let agent = chain.reviewers[chain.stage.reviewer - 1].clone();
        let text = if chain.stage.judges() {
            review::judge_prompt(&chain.work, &chain.findings)
        } else {
            review::propose_prompt(&chain.work)
        };
        let run = Arc::new(Run::create(
            &self.repo.runs_dir(),
            &agent.name,
            &chain.notes,
            &text,
            Some(chain.stage.clone()),
        )?);
        slot.run = Some(Arc::clone(&run));
        let replaced = slot.release();
        let pending = Pending {
            agent,
            run,
            // A review marks no note sent: the notes went to the reviewed run.
            ids: Vec::new(),
            prompt: Prompt {
                text,
                files: chain.files.clone(),
            },
            generation: slot.generation,
        };
        slot.chain = Some(chain);
        self.start(pending);
        Ok(replaced)
    }

    /// Starts the review's next stage, once `goes_on` moved the chain to it as stage `ended`
    /// ended. Runs on a thread of its own: the stage's agent is replaced, and dropping an
    /// agent from its own reader thread would wait on itself.
    fn advance(self: &Arc<Self>, ended: &str) {
        let mut slot = self.slot();
        // A send, a stop, or another review meanwhile took the chain, or replaced its run.
        let current = slot.run.as_ref().map(|run| run.state().header.id.clone());
        let Some(chain) = slot.chain.take() else {
            return;
        };
        if current.as_deref() != Some(ended) {
            slot.chain = Some(chain);
            return;
        }
        match self.start_stage(&mut slot, chain) {
            Ok(replaced) => {
                drop(slot);
                drop(replaced);
            }
            Err(error) => {
                // The stage that ended says why the review stopped there.
                if let Some(run) = &slot.run {
                    run.state().problem.get_or_insert(format!(
                        "the next reviewer could not start: {}",
                        error_chain(&error)
                    ));
                }
                drop(slot);
            }
        }
        (self.notify)();
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
        let mut released = Held::None;
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
                // reported over this.
                released = slot.release();
                run.record(Entry::Failed {
                    error: "stopped: the agent did not stop when asked, so Postil ended it"
                        .to_owned(),
                });
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
        drop(released);
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

    /// The latest run, with the entries from number `after` on: this window's run, or the
    /// newest readable log on disk when this window has run nothing yet. `None` when there is
    /// no run.
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
        {
            let mut slot = self.slot();
            if slot
                .run
                .as_ref()
                .is_some_and(|run| deleted.contains(&run.state().header.id))
            {
                slot.run = None;
            }
        }
        (self.notify)();
        Ok(())
    }

    /// Stops the agent, running or starting, and ignores anything it would still report: the
    /// project is closing, or the app quits.
    pub(crate) fn close(&self) {
        let released = {
            let mut slot = self.slot();
            slot.closed = true;
            slot.chain = None;
            slot.release()
        };
        drop(released);
    }

    fn slot(&self) -> MutexGuard<'_, Slot> {
        lock(&self.slot)
    }

    /// Starts the agent on its own thread, then sends the prompt, unless the agent was let go
    /// meanwhile (the run cancelled, the project closed).
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
            // Blocks until the login shell has said where programs are, a few seconds at most
            // and only once per app.
            let search_path = runner.agents.wait();
            if runner.slot().let_go(generation) {
                return;
            }
            let launch = Launch {
                command: &pending.agent.command,
                cwd: runner.repo.root(),
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
                    runner.begin(&agent, &pending.run, &pending.ids, &pending.prompt);
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

    /// Sends the prompt to a running agent. Its notes are marked sent once the agent shows it
    /// took the turn ([`Run::record_event`]).
    fn begin(&self, agent: &Agent, run: &Run, ids: &[String], prompt: &Prompt) {
        if !ids.is_empty() {
            run.state().unmarked = Some(ids.to_vec());
        }
        let blocks = prompt_blocks(prompt, self.repo.root());
        if let Err(error) = agent.prompt(&blocks) {
            return run.record(Entry::Failed {
                error: error_chain(&error),
            });
        }
        run.record(Entry::Started);
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
    let (run, released, answer, stage) = {
        let mut slot = runner.slot();
        if slot.let_go(generation) {
            return;
        }
        // The review stage this run is, while its review goes on.
        let stage = slot
            .run
            .as_ref()
            .and_then(|run| run.state().header.review.clone())
            .filter(|stage| {
                slot.chain
                    .as_ref()
                    .is_some_and(|chain| chain.stage == *stage)
            });
        // Answered before the request is logged, and logged with it at once: a page that read
        // the request alone would show the run waiting for the engineer.
        // From the run's header, which never changes: a Stop that ends the chain does not lift
        // it for the rest of the proposer's turn.
        let read_only = slot.run.as_ref().is_some_and(|run| {
            run.state()
                .header
                .review
                .as_ref()
                .is_some_and(|stage| !stage.judges())
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
        let found = slot.chain.as_ref().map_or(0, |chain| chain.findings.len());
        (
            slot.run.clone(),
            released,
            answer,
            stage.map(|stage| (stage, found)),
        )
    };
    let mut next = None;
    if let Some(run) = run {
        // A proposer's findings are read from its reply as its turn ends, and logged before
        // the end, while the run still takes entries.
        // Whether the review goes on is decided here, before the stage's end is logged: a
        // reader that sees the end sees the decision. Only starting the next stage waits for
        // a thread of its own.
        if ended_well && let Some((stage, found)) = &stage {
            let findings = end_stage(&run, stage, *found);
            let mut slot = runner.slot();
            let goes = match (findings, slot.chain.as_mut()) {
                (Some(findings), Some(chain)) => goes_on(chain, findings),
                _ => false,
            };
            if goes {
                next = Some(run.state().header.id.clone());
            } else {
                slot.chain = None;
            }
        }
        if let Some(ids) = run.record_event(event, answer, runner.repo.root()) {
            runner.mark_sent(&run, &ids);
        }
    }
    // Dropped outside the lock: dropping an agent waits for its process to be reaped.
    drop(released);
    (runner.notify)();
    if let Some(ended) = next {
        let next = Arc::clone(&runner);
        let advanced = std::thread::Builder::new()
            .name("review-next".to_owned())
            .spawn(move || next.advance(&ended));
        if let Err(error) = advanced {
            let mut slot = runner.slot();
            slot.chain = None;
            if let Some(run) = &slot.run {
                run.state().problem.get_or_insert(format!(
                    "the next reviewer could not start: no thread for it ({error})"
                ));
            }
        }
    }
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
    let option = if read_only && !reads {
        reject_option(&request.options)
    } else {
        allow_option(&request.options)
    }?;
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
/// logged. Returns them for the next stage; `None` when the review ends here: this was the
/// judge, or a proposer changed files.
fn end_stage(run: &Run, stage: &Stage, found: usize) -> Option<Vec<Finding>> {
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
    // a changed tree for the reviewed one.
    let edited = !run.state().work().diffs.is_empty();
    let problem = if edited {
        Some(format!(
            "reviewer {} changed files, though it may only read: the review stops here",
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

/// The option that rejects a permission request: once, else always. `None` when the agent
/// offers neither; the request then waits for the engineer.
fn reject_option(options: &[PermissionOption]) -> Option<String> {
    [PermissionKind::RejectOnce, PermissionKind::RejectAlways]
        .into_iter()
        .find_map(|kind| options.iter().find(|option| option.kind == kind))
        .map(|option| option.option_id.clone())
}

/// The option that allows a permission request: once, else always. `None` when the agent
/// offers neither; the request then waits for the engineer, as before auto-allow.
fn allow_option(options: &[PermissionOption]) -> Option<String> {
    [PermissionKind::AllowOnce, PermissionKind::AllowAlways]
        .into_iter()
        .find_map(|kind| options.iter().find(|option| option.kind == kind))
        .map(|option| option.option_id.clone())
}

/// One run and its log.
pub(crate) struct Run {
    file: PathBuf,
    state: Mutex<RunState>,
}

struct RunState {
    header: Header,
    entries: Vec<Entry>,
    /// The log, open for appending while live; `None` after it ended or a write failed.
    log: Option<File>,
    /// The exclusive lock on `runs/active.lock`, held from the run's creation until it ends:
    /// whether it is held is whether the run is live (`RunState::live`). A log read from disk
    /// has none. The file's text is the highest run number given (`last_run`).
    active: Option<File>,
    // Kept as each entry is recorded, so a status needs no scan of the entries.
    started: bool,
    cancel_requested: bool,
    /// When this process asked the agent to stop, for [`FORCE_AFTER`]. Not logged.
    cancel_asked: Option<Instant>,
    /// The notes sent, until the agent shows it took the turn; then they are marked sent.
    unmarked: Option<Vec<String>>,
    end: Option<Status>,
    // Sets: a cancel can log a request's answer before the request itself.
    requested: BTreeSet<usize>,
    answered: BTreeSet<usize>,
    /// Why the log is incomplete, the notes were not marked sent, or a review stopped here.
    problem: Option<String>,
}

/// The first line of a run log.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Header {
    version: u32,
    /// `R1`, `R2`: the log's file name.
    id: String,
    /// The agent's name in the settings.
    agent: String,
    /// The ids of the notes sent.
    notes: Vec<String>,
    /// The prompt's text, as the agent received it.
    prompt: String,
    /// Seconds since the Unix epoch.
    started_at: u64,
    /// Set when the run is a reviewer's (`review.rs`): which run it reviews, and its place in
    /// the chain. Absent for a run of notes, and in every log written before reviews.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    review: Option<Stage>,
}

/// The one field of a header that every log version has. Unknown fields are ignored here on
/// purpose.
#[derive(Deserialize)]
struct VersionOnly {
    version: u32,
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
    },
    /// A tool call changed; only the fields that changed are set.
    ToolUpdate {
        id: String,
        title: Option<String>,
        tool: Option<ToolKind>,
        status: Option<ToolCallStatus>,
        places: Option<Vec<Place>>,
        output: Option<Vec<Output>>,
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
    /// The log has no end and this process does not run it: the app quit or the project was
    /// closed while it ran, or another window is running it.
    Unfinished,
}

/// A run as the page gets it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunView {
    id: String,
    agent: String,
    notes: Vec<String>,
    prompt: String,
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
}

impl Run {
    /// Creates the log for a new run in `dir`, with the next free id, takes the working
    /// tree's run lock, and writes the header.
    fn create(
        dir: &Path,
        agent: &str,
        notes: &[String],
        prompt: &str,
        review: Option<Stage>,
    ) -> Result<Self> {
        let error = |path: &Path| {
            let path = path.to_owned();
            move |source| Error::WriteRun { path, source }
        };
        fs::create_dir_all(dir).map_err(error(dir))?;
        let active_path = dir.join("active.lock");
        let mut active = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&active_path)
            .map_err(error(&active_path))?;
        take_run_lock(&active).map_err(|source| match source {
            TryLockError::WouldBlock => Error::RunElsewhere,
            TryLockError::Error(source) => Error::WriteRun {
                path: active_path.clone(),
                source,
            },
        })?;

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
        Ok(Self {
            file,
            state: Mutex::new(RunState::new(header, Some(log), Some(active))),
        })
    }

    /// The newest readable run log in `dir`, read as a run this process does not run; `None`
    /// when there is none. A newer log that cannot be read is skipped, and the run shown says
    /// why.
    fn latest(dir: &Path) -> Result<Option<Self>> {
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
    fn read(file: &Path) -> std::result::Result<Self, String> {
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
            state: Mutex::new(state),
        })
    }

    fn state(&self) -> MutexGuard<'_, RunState> {
        lock(&self.state)
    }

    /// Adds an entry to the run and its log. A run that ended takes no more entries.
    fn record(&self, entry: Entry) {
        self.record_all([entry]);
    }

    /// Records `entries` in order, under one hold of the state: a reader sees all or none.
    fn record_all(&self, entries: impl IntoIterator<Item = Entry>) {
        let mut state = self.state();
        if !state.live() {
            return;
        }
        for entry in entries {
            if let Some(log) = &mut state.log
                && let Err(error) = log.write_all(&json_line(&entry))
            {
                state.log = None;
                state.problem.get_or_insert(format!(
                    "the run log {} could not be written, so this run will not be shown whole \
                     after a restart: {error}",
                    self.file.display()
                ));
            }
            state.apply(&entry);
            state.entries.push(entry);
        }
    }

    /// Records an agent's event, and returns the notes to mark sent now: the first sign that
    /// the agent took the turn (an update, a permission request, or a stop other than a
    /// refusal) releases them. A turn that fails or is refused at once leaves them open.
    /// `answer`, the event's auto-answer, is recorded with it, at once.
    fn record_event(
        &self,
        event: Event,
        answer: Option<Entry>,
        root: &Path,
    ) -> Option<Vec<String>> {
        let entry = match event {
            Event::Update(update) => entry_of(update, root)?,
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
            Event::TurnEnded(TurnEnd::Stopped(reason)) => Entry::Ended { reason },
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
        let live = self.state().live();
        self.record_all(std::iter::once(entry).chain(answer));
        if took_turn && live {
            self.state().unmarked.take()
        } else {
            None
        }
    }

    fn view(&self, after: usize) -> RunView {
        let state = self.state();
        RunView {
            id: state.header.id.clone(),
            agent: state.header.agent.clone(),
            notes: state.header.notes.clone(),
            prompt: state.header.prompt.clone(),
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
    fn new(header: Header, log: Option<File>, active: Option<File>) -> Self {
        Self {
            header,
            entries: Vec::new(),
            log,
            active,
            started: false,
            cancel_requested: false,
            cancel_asked: None,
            unmarked: None,
            end: None,
            requested: BTreeSet::new(),
            answered: BTreeSet::new(),
            problem: None,
        }
    }

    /// The agent's reply: its message parts joined, a blank line between messages.
    fn reply(&self) -> String {
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
    fn work(&self) -> Work {
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
    fn live(&self) -> bool {
        self.active.is_some()
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
                // Another window may start a run now.
                self.active = None;
            }
            Entry::Message { .. }
            | Entry::Thought { .. }
            | Entry::Tool { .. }
            | Entry::ToolUpdate { .. }
            | Entry::Plan { .. }
            | Entry::Unreadable { .. }
            | Entry::Findings { .. } => {}
        }
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

/// Takes the exclusive lock on `runs/active.lock`, or reports that another window holds it.
///
/// A `flock` lock belongs to the open file, and a process this app forks (git, an agent)
/// holds a copy of every open file from its fork until its exec closes them. A lock released
/// a moment ago can so look held for microseconds, which a send right after a run ends hit in
/// the tests. The lock is therefore tried for up to [`LOCK_PATIENCE`]; a window that really
/// runs an agent holds it far longer.
fn take_run_lock(active: &File) -> std::result::Result<(), TryLockError> {
    let deadline = Instant::now() + LOCK_PATIENCE;
    loop {
        match active.try_lock() {
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            result => return result,
        }
    }
}

/// The numbers of the run logs in `dir`; none when there is no `dir`.
fn run_numbers(dir: &Path) -> io::Result<Vec<u64>> {
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
fn run_number(id: &str) -> Option<u64> {
    let number = id.strip_prefix('R')?.parse::<u64>().ok()?;
    (format!("R{number}") == id).then_some(number)
}

/// A run log's first line, its header. The version is read on its own first: a newer header
/// would otherwise fail on its first new field, with a message that hides the real reason.
fn parse_header(first: &[u8]) -> std::result::Result<Header, String> {
    let VersionOnly { version } = serde_json::from_slice(first)
        .map_err(|error| format!("its first line is not a run header ({error})"))?;
    if !(1..=LOG_VERSION).contains(&version) {
        return Err(format!(
            "it is version {version}, and this build of Postil reads versions 1 to {LOG_VERSION}"
        ));
    }
    serde_json::from_slice(first)
        .map_err(|error| format!("its first line is not a run header ({error})"))
}

/// The header of the run log `file`, its first line only: a long run's log is not read whole.
fn read_header(file: &Path) -> std::result::Result<Header, String> {
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

/// A session update as a log entry; `None` for what the run panel does not show.
fn entry_of(update: SessionUpdate, root: &Path) -> Option<Entry> {
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
        SessionUpdate::ToolCall(call) => Entry::Tool {
            id: call.tool_call_id,
            title: call.title,
            tool: call.kind,
            status: call.status,
            places: places(call.locations, root),
            output: outputs(call.content, root),
        },
        SessionUpdate::ToolCallUpdate(update) => Entry::ToolUpdate {
            id: update.tool_call_id,
            title: update.title,
            tool: update.kind,
            status: update.status,
            places: update.locations.map(|locations| places(locations, root)),
            output: update.content.map(|content| outputs(content, root)),
        },
        SessionUpdate::Plan { entries } => Entry::Plan {
            steps: entries
                .into_iter()
                .map(|entry| Step {
                    content: entry.content,
                    status: entry.status,
                })
                .collect(),
        },
        // An image or a link in a message, and updates the panel does not show.
        SessionUpdate::AgentMessageChunk { .. }
        | SessionUpdate::AgentThoughtChunk { .. }
        | SessionUpdate::Other => return None,
    };
    Some(entry)
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

fn json_line<T: Serialize>(value: &T) -> Vec<u8> {
    let mut line = serde_json::to_vec(value).expect("run entries are plain data");
    line.push(b'\n');
    line
}

/// Locks a runner's or a run's state. Poisoning is ignored: each change under these locks
/// is a push or an assignment that a panic cannot leave half done.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_names_the_run_but_never_shows_its_prompt() {
        let dir = std::env::temp_dir().join(format!("postil-run-debug-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let run = Run::create(&dir, "Fake", &["N1".to_owned()], "secret prompt", None).unwrap();
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
        drop(fs::remove_dir_all(&dir));
    }

    #[test]
    fn after_a_failed_write_the_log_is_not_written_again_and_the_run_says_so() {
        let dir = std::env::temp_dir().join(format!("postil-run-log-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let run = Run::create(&dir, "Fake", &["N1".to_owned()], "p", None).unwrap();
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
        drop(fs::remove_dir_all(&dir));
    }

    #[test]
    fn the_work_holds_each_finished_call_s_last_diffs_and_the_reply_by_message() {
        let dir = std::env::temp_dir().join(format!("postil-run-work-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let run = Run::create(&dir, "Fake", &["N1".to_owned()], "p", None).unwrap();
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
        drop(fs::remove_dir_all(&dir));
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
