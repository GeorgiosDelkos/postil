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
//! starting is killed instead, and the run ends at once: a start can take minutes. Asked a
//! second time, Stop kills an agent that did not stop, so a hung agent never holds the
//! working tree.
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
//! The app never rewrites a log, and reads one only to show the latest run after a restart.
//! A log with no end is shown as unfinished: the app quit or the project was closed while
//! it ran, or another window is running it.
//!
//! # Threads and locks
//!
//! Requests from the page run on the main thread, the agent's events on its reader thread,
//! and an agent's start on a thread of its own. Two locks, always taken in this order: the
//! runner's [`Slot`], then a run's state. The page is told about every change through
//! `notify`, and fetches what it has not seen.

use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, File, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::acp::{
    Agent, ContentBlock, Event, Launch, Location, PermissionKind, PlanEntryStatus, SessionUpdate,
    StopReason, Stopper, ToolCallContent, ToolCallStatus, ToolKind, TurnEnd,
};
use crate::agents::Scanner;
use crate::error::{Error, Result, error_chain};
use crate::notes::{self, Prompt};
use crate::repo::Repo;

/// How long an agent may take to start, and then to open a session. Claude's adapter took
/// 4.3 s to open a session with npx's cache warm (2026-11-23); a first `npx` run downloads
/// the adapter, which takes tens of seconds on a slow link. Past this, the run fails with
/// the hint to run the agent in a terminal (an agent waiting for a browser sign-in never
/// answers). Stop ends the wait sooner.
const START_TIMEOUT: Duration = Duration::from_secs(120);

/// The run log's format version, in its header line.
const LOG_VERSION: u32 = 1;

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

/// What a runner holds now.
#[derive(Debug, Default)]
struct Slot {
    /// The agent kept between runs.
    agent: Option<(AgentChoice, Agent)>,
    /// Stops the agent being started, while its session is being opened.
    starting: Option<Stopper>,
    /// The number of the agent started last. An agent's events and its start carry the
    /// number it was started with: when it differs, the agent was replaced, stopped, or its
    /// run cancelled, and what it reports is dropped.
    generation: u64,
    /// This window's latest run.
    run: Option<Arc<Run>>,
    /// Set when the project closed: an agent that finishes starting afterwards is stopped.
    closed: bool,
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
    /// the one kept from the last run and still alive. Returns at once; the run reports its
    /// progress through its log.
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
        let run = Arc::new(Run::create(
            &self.repo.runs_dir(),
            &agent.name,
            ids,
            &prompt.text,
        )?);
        slot.run = Some(Arc::clone(&run));

        let kept = slot
            .agent
            .as_ref()
            .filter(|(kept, live)| *kept == agent && live.is_alive());
        if let Some((_, live)) = kept {
            self.begin(live, &run, ids, &prompt);
        } else {
            // Dropping the agent kept from the last run stops it.
            slot.agent = None;
            slot.generation += 1;
            let pending = Pending {
                agent,
                run,
                ids: ids.to_vec(),
                prompt,
                generation: slot.generation,
            };
            self.start(pending);
        }
        drop(slot);
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
            (state.started, state.cancel_requested)
        };
        let mut result = Ok(());
        if !started {
            // The agent is still starting: it is stopped, and its start finds it replaced.
            slot.generation += 1;
            if let Some(starting) = slot.starting.take() {
                starting.stop();
            }
            run.record(Entry::CancelRequested);
            run.record(Entry::Ended {
                reason: StopReason::Cancelled,
            });
        } else if asked {
            // Asked twice: the agent did not stop when asked, so it is ended, and its exit is
            // not reported over this.
            slot.generation += 1;
            slot.agent = None;
            run.record(Entry::Failed {
                error: "stopped: the agent did not stop when asked, so Postil ended it".to_owned(),
            });
        } else {
            run.record(Entry::CancelRequested);
            if let Some((_, agent)) = &slot.agent {
                // The requests this answered `cancelled` on the wire are closed in the log too,
                // including one whose event is still on its way.
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
        drop(slot);
        (self.notify)();
        result
    }

    /// Answers the run's permission request `request` (its number, as the log gives it) with
    /// the option `option`.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchPermission`] when that request is not open in this run; as
    /// [`Agent::answer_permission`].
    pub(crate) fn answer(&self, request: usize, option: &str) -> Result<()> {
        let slot = self.slot();
        let (Some(run), Some((_, agent))) = (&slot.run, &slot.agent) else {
            return Err(Error::NoSuchPermission);
        };
        if !run.state().is_open(request) {
            return Err(Error::NoSuchPermission);
        }
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

    /// Stops the agent, running or starting, and ignores anything it would still report: the
    /// project is closing, or the app quits.
    pub(crate) fn close(&self) {
        let mut slot = self.slot();
        slot.closed = true;
        slot.agent = None;
        if let Some(starting) = slot.starting.take() {
            starting.stop();
        }
    }

    fn slot(&self) -> MutexGuard<'_, Slot> {
        lock(&self.slot)
    }

    /// Starts the agent on its own thread, then sends the prompt, unless the run was
    /// cancelled or the project closed meanwhile.
    fn start(self: &Arc<Self>, pending: Pending) {
        let runner = Arc::clone(self);
        let run = Arc::clone(&pending.run);
        let start = move || {
            let failed = |error: &Error| {
                pending.run.record(Entry::Failed {
                    error: error_chain(error),
                });
            };
            // Blocks until the login shell has said where programs are, a few seconds at most
            // and only once per app.
            let search_path = runner.agents.wait();
            let launch = Launch {
                command: &pending.agent.command,
                cwd: runner.repo.root(),
                search_path: &search_path,
                timeout: START_TIMEOUT,
            };
            // Weak: the runner owns the agent, whose reader thread owns this callback.
            let events = Arc::downgrade(&runner);
            let generation = pending.generation;
            let spawned = Agent::spawn(&launch, move |event| on_event(&events, generation, event));
            let starting = match spawned {
                Ok(starting) => starting,
                Err(error) => {
                    failed(&error);
                    return (runner.notify)();
                }
            };

            // Stoppable from here on; a cancel or close that came first wins.
            {
                let mut slot = runner.slot();
                if slot.closed || slot.generation != generation {
                    return;
                }
                slot.starting = Some(starting.stopper());
            }
            let opened = starting.open_session();

            let mut slot = runner.slot();
            if slot.generation != generation || slot.closed {
                // Cancelled or closed while starting: the run already says so, and the agent,
                // if it started after all, is dropped here, which stops it.
                return;
            }
            slot.starting = None;
            match opened {
                Err(error) => failed(&error),
                Ok(agent) => {
                    runner.begin(&agent, &pending.run, &pending.ids, &pending.prompt);
                    slot.agent = Some((pending.agent, agent));
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

    /// Sends the prompt to a running agent and marks the notes sent.
    fn begin(&self, agent: &Agent, run: &Run, ids: &[String], prompt: &Prompt) {
        let blocks = prompt_blocks(prompt, self.repo.root());
        if let Err(error) = agent.prompt(&blocks) {
            return run.record(Entry::Failed {
                error: error_chain(&error),
            });
        }
        run.record(Entry::Started);
        // The prompt is out: a note that cannot be marked would be sent again by mistake
        // later, so the run says so.
        let id = run.state().header.id.clone();
        if let Err(error) = notes::mark_sent(&self.repo, ids, &id) {
            run.state().problem.get_or_insert(format!(
                "the notes were sent but could not be marked sent: {}",
                error_chain(&error)
            ));
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
/// was replaced or stopped, or the project closed.
fn on_event(runner: &Weak<Runner>, generation: u64, event: Event) {
    let Some(runner) = runner.upgrade() else {
        return;
    };
    let run = {
        let mut slot = runner.slot();
        if slot.closed || slot.generation != generation {
            return;
        }
        if matches!(event, Event::Exited { .. }) {
            // The next send starts the agent again.
            slot.agent = None;
        }
        slot.run.clone()
    };
    if let Some(run) = run {
        run.record_event(event, runner.repo.root());
    }
    (runner.notify)();
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
    /// has none.
    active: Option<File>,
    // Kept as each entry is recorded, so a status needs no scan of the entries.
    started: bool,
    cancel_requested: bool,
    end: Option<Status>,
    requested: BTreeSet<usize>,
    answered: BTreeSet<usize>,
    /// Why the log is incomplete, or the notes were not marked sent.
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
    /// A permission request was answered; `option` is `None` when the run was cancelled.
    Answer {
        request: usize,
        option: Option<String>,
    },
    /// The agent sent something the app could not read; the run goes on.
    Unreadable { problem: String },
    /// The engineer asked the agent to stop.
    CancelRequested,
    /// The turn ended.
    Ended { reason: StopReason },
    /// The run failed: the agent did not start, refused the prompt, or exited.
    Failed { error: String },
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
}

impl Run {
    /// Creates the log for a new run in `dir`, with the next free id, takes the working
    /// tree's run lock, and writes the header.
    fn create(dir: &Path, agent: &str, notes: &[String], prompt: &str) -> Result<Self> {
        let error = |path: &Path| {
            let path = path.to_owned();
            move |source| Error::WriteRun { path, source }
        };
        fs::create_dir_all(dir).map_err(error(dir))?;
        let active_path = dir.join("active.lock");
        let active = File::options()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&active_path)
            .map_err(error(&active_path))?;
        match active.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(Error::RunElsewhere),
            Err(TryLockError::Error(source)) => {
                return Err(Error::WriteRun {
                    path: active_path,
                    source,
                });
            }
        }

        let mut number = highest_run(dir).map_err(error(dir))? + 1;
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
        };
        if let Err(source) = log.write_all(&json_line(&header)) {
            // A log without its header would be skipped as unreadable on every restart.
            drop(fs::remove_file(&file));
            return Err(Error::WriteRun { path: file, source });
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
        let first = lines.next().unwrap_or_default();
        // The version is read on its own first: a newer header would otherwise fail on its
        // first new field, with a message that hides the real reason.
        let VersionOnly { version } = serde_json::from_slice(first)
            .map_err(|error| format!("its first line is not a run header ({error})"))?;
        if version != LOG_VERSION {
            return Err(format!(
                "it is version {version}, and this build of Postil reads version {LOG_VERSION}"
            ));
        }
        let header: Header = serde_json::from_slice(first)
            .map_err(|error| format!("its first line is not a run header ({error})"))?;

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
        let mut state = self.state();
        if !state.live() {
            return;
        }
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

    /// Records an agent's event.
    fn record_event(&self, event: Event, root: &Path) {
        let entry = match event {
            Event::Update(update) => match entry_of(update, root) {
                Some(entry) => entry,
                None => return,
            },
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
            Event::TurnEnded(TurnEnd::Stopped(reason)) => Entry::Ended { reason },
            Event::TurnEnded(TurnEnd::Failed(error)) => Entry::Failed { error },
            Event::Unreadable(problem) => Entry::Unreadable { problem },
            Event::Exited { stderr } => Entry::Failed {
                error: Error::AgentExited { stderr }.to_string(),
            },
        };
        self.record(entry);
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
            end: None,
            requested: BTreeSet::new(),
            answered: BTreeSet::new(),
            problem: None,
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
            Entry::Permission { request, .. } => {
                self.requested.insert(*request);
            }
            Entry::Answer { request, .. } => {
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
            | Entry::Unreadable { .. } => {}
        }
    }

    /// Whether the permission request `request` was logged and not answered. An answer can
    /// be logged before its request (a cancel answers one whose event is on its way), so
    /// the two are kept as sets.
    fn is_open(&self, request: usize) -> bool {
        self.requested.contains(&request) && !self.answered.contains(&request)
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
            .and_then(|name| name.strip_prefix('R'))
            .and_then(|name| name.strip_suffix(".jsonl"))
            .and_then(|digits| digits.parse::<u64>().ok());
        numbers.extend(number);
    }
    Ok(numbers)
}

/// The highest run number among the logs in `dir`; 0 when there is none.
fn highest_run(dir: &Path) -> io::Result<u64> {
    Ok(run_numbers(dir)?.into_iter().max().unwrap_or(0))
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
    fn after_a_failed_write_the_log_is_not_written_again_and_the_run_says_so() {
        let dir = std::env::temp_dir().join(format!("postil-run-log-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let run = Run::create(&dir, "Fake", &["N1".to_owned()], "p").unwrap();
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
}
