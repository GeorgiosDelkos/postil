//! Runs: notes sent to the agent as one prompt, and everything the agent reports back.
//!
//! A [`Runner`] belongs to the open project. It keeps the agent it started alive between
//! runs, with its session, so a later prompt reaches the same conversation; the owner chose
//! this over a fresh agent per send on 2026-11-23, for milestone 4's follow-up notes. The
//! agent is replaced when a send names another agent, and stopped when the project closes.
//! One run at a time: two agents writing one working tree conflict (docs/design.md, "Not in
//! scope").
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
//! The app never rewrites a log, and reads one only to show the latest run after a restart.
//! A run whose log has no end is shown as interrupted: the app quit while it ran.
//!
//! # Threads and locks
//!
//! Requests from the page run on the main thread, the agent's events on its reader thread,
//! and an agent's start on a thread of its own, because a start can take minutes (an agent
//! that waits for a sign-in). Two locks, always taken in this order: the runner's [`Slot`],
//! then a run's state. The page is told about every change through `notify`, and fetches
//! what it has not seen.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::acp::{
    Agent, ContentBlock, Event, Launch, Location, PermissionKind, PlanEntryStatus, RequestId,
    SessionUpdate, StopReason, ToolCallContent, ToolCallStatus, ToolKind, TurnEnd,
};
use crate::agents::Scanner;
use crate::error::{Error, Result, error_chain};
use crate::notes::{self, Anchor, Note};
use crate::repo::Repo;

/// How long an agent may take to start, and then to open a session. Claude's adapter took
/// 4.3 s to open a session with npx's cache warm (2026-11-23); a first `npx` run downloads
/// the adapter, which takes tens of seconds on a slow link. Past this, the run fails with
/// the hint to run the agent in a terminal (an agent waiting for a browser sign-in never
/// answers).
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
    /// The number of the agent started last. An agent's events carry the number it was
    /// started with, so the late events of a replaced agent are dropped.
    generation: u64,
    /// This window's latest run.
    run: Option<Arc<Run>>,
    /// Set when the project closed: an agent that finishes starting afterwards is stopped.
    closed: bool,
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
    /// As [`notes::select`].
    pub(crate) fn preview(&self, ids: &[String]) -> Result<String> {
        let notes = notes::select(&self.repo, ids)?;
        Ok(prompt_text(&notes))
    }

    /// Starts a run: sends the notes `ids` to `agent`, starting the agent first unless it is
    /// the one kept from the last run. Returns at once; the run reports its progress through
    /// its log.
    ///
    /// # Errors
    ///
    /// [`Error::TurnRunning`] while a run is under way; as [`notes::select`]; and
    /// [`Error::WriteRun`] when the run log cannot be created.
    pub(crate) fn send(self: &Arc<Self>, agent: AgentChoice, ids: &[String]) -> Result<()> {
        let mut slot = self.slot();
        if slot.run.as_ref().is_some_and(|run| run.state().is_live()) {
            return Err(Error::TurnRunning);
        }
        let notes = notes::select(&self.repo, ids)?;
        let prompt = Prompt {
            text: prompt_text(&notes),
            ids: ids.to_vec(),
            files: files_of(&notes),
        };
        let header = Header {
            version: LOG_VERSION,
            id: String::new(),
            agent: agent.name.clone(),
            notes: prompt.ids.clone(),
            prompt: prompt.text.clone(),
            started_at: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |since| since.as_secs()),
        };
        let run = Arc::new(Run::create(&self.repo.runs_dir(), header)?);
        slot.run = Some(Arc::clone(&run));

        match &slot.agent {
            Some((kept, live)) if *kept == agent => self.begin(live, &run, &prompt),
            _ => {
                // Dropping the agent kept from the last run stops it.
                slot.agent = None;
                slot.generation += 1;
                self.start(agent, &run, prompt, slot.generation);
            }
        }
        drop(slot);
        (self.notify)();
        Ok(())
    }

    /// Asks the agent to stop the run. A run whose agent is still starting ends as soon as
    /// the agent is up.
    ///
    /// # Errors
    ///
    /// [`Error::BadRequest`] when no run is under way; [`Error::AgentExited`] when the agent
    /// is gone (its exit then ends the run).
    pub(crate) fn cancel(&self) -> Result<()> {
        let slot = self.slot();
        let run = slot
            .run
            .as_ref()
            .filter(|run| run.state().is_live())
            .ok_or_else(|| Error::BadRequest("no run is under way".to_owned()))?;
        let started = run.request_cancel();
        let cancelled = match (&slot.agent, started) {
            (Some((_, agent)), true) => agent.cancel(),
            _ => Ok(()),
        };
        drop(slot);
        (self.notify)();
        cancelled
    }

    /// Answers the run's permission request `request` (its number in the log) with the
    /// option `option`.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchPermission`] when that request is not open; as
    /// [`Agent::answer_permission`].
    pub(crate) fn answer(&self, request: usize, option: &str) -> Result<()> {
        let slot = self.slot();
        let (Some(run), Some((_, agent))) = (&slot.run, &slot.agent) else {
            return Err(Error::NoSuchPermission);
        };
        let id = run.open_permission(request)?;
        // The agent checks the option against what it offered; only an accepted answer
        // closes the request, so a refused one can be answered again.
        agent.answer_permission(&id, option)?;
        run.answered(request, option);
        drop(slot);
        (self.notify)();
        Ok(())
    }

    /// The latest run, with the entries from number `after` on: this window's run, or the
    /// newest log on disk when this window has run nothing yet. `None` when there is no run.
    ///
    /// # Errors
    ///
    /// [`Error::ReadRun`] when the newest log on disk cannot be read.
    pub(crate) fn view(&self, after: usize) -> Result<Option<RunView>> {
        let run = self.slot().run.clone();
        match run {
            Some(run) => Ok(Some(run.view(after))),
            None => Ok(Run::latest(&self.repo.runs_dir())?.map(|run| run.view(after))),
        }
    }

    /// Stops the agent and ignores anything it would still report: the project is closing.
    pub(crate) fn close(&self) {
        let mut slot = self.slot();
        slot.closed = true;
        slot.agent = None;
    }

    fn slot(&self) -> MutexGuard<'_, Slot> {
        lock(&self.slot)
    }

    /// Starts `agent` on its own thread, then sends the prompt, unless the project closed or
    /// the run was cancelled meanwhile.
    fn start(
        self: &Arc<Self>,
        agent: AgentChoice,
        run: &Arc<Run>,
        prompt: Prompt,
        generation: u64,
    ) {
        let runner = Arc::clone(self);
        let starting = Arc::clone(run);
        let start = move || {
            let search_path: OsString = runner.agents.wait();
            // Weak: the runner owns the agent, whose reader thread owns this callback.
            let events = Arc::downgrade(&runner);
            let launch = Launch {
                command: &agent.command,
                cwd: runner.repo.root(),
                search_path: &search_path,
                timeout: START_TIMEOUT,
            };
            let started = Agent::start(&launch, move |event| on_event(&events, generation, event));

            let mut slot = runner.slot();
            match started {
                Err(error) => starting.record(Entry::Failed {
                    error: error_chain(&error),
                }),
                Ok(live) if slot.closed || slot.generation != generation => {
                    drop(live);
                    starting.record(Entry::Failed {
                        error: "the project was closed before the agent started".to_owned(),
                    });
                }
                Ok(live) => {
                    if starting.state().cancel_requested {
                        starting.record(Entry::Ended {
                            reason: StopReason::Cancelled,
                        });
                    } else {
                        runner.begin(&live, &starting, &prompt);
                    }
                    slot.agent = Some((agent, live));
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
    fn begin(&self, agent: &Agent, run: &Run, prompt: &Prompt) {
        let blocks = prompt_blocks(prompt, self.repo.root());
        if let Err(error) = agent.prompt(&blocks) {
            return run.record(Entry::Failed {
                error: error_chain(&error),
            });
        }
        run.record(Entry::Started);
        // The prompt is out: a note that cannot be marked would be sent again by mistake
        // later, so the run says so.
        if let Err(error) = notes::mark_sent(&self.repo, &prompt.ids, &run.id()) {
            run.set_problem(format!(
                "the notes were sent but could not be marked sent: {}",
                error_chain(&error)
            ));
        }
    }
}

impl std::fmt::Debug for Runner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runner")
            .field("root", &self.repo.root())
            .field("slot", &*self.slot())
            .finish_non_exhaustive()
    }
}

/// An agent's event, on its reader thread: recorded in the current run unless the agent
/// was replaced or the project closed.
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

/// The notes of a run, as sent.
#[derive(Debug)]
struct Prompt {
    text: String,
    ids: Vec<String>,
    /// The notes' files, each once, in the order of the notes.
    files: Vec<String>,
}

/// One run and its log.
#[derive(Debug)]
pub(crate) struct Run {
    file: PathBuf,
    state: Mutex<RunState>,
}

#[derive(Debug)]
struct RunState {
    header: Header,
    entries: Vec<Entry>,
    /// The agent's id of each permission request, by its number in the log; `None` once it
    /// is answered.
    permissions: Vec<Option<RequestId>>,
    /// The open log while this process runs the run; `None` once it ended, and for a log
    /// read from disk.
    log: Option<File>,
    cancel_requested: bool,
    /// Why the log could not be written in full, or the notes not marked sent.
    problem: Option<String>,
}

/// The first line of a run log.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Part of the agent's reply; consecutive parts form one message.
    Message { text: String },
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
    /// The agent asks to make a tool call. `request` is its number among the run's requests.
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
    /// The log has no end: the app quit while the run went on.
    Interrupted,
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
    /// Creates the log for a new run in `dir`, with the next free id, and writes its header.
    fn create(dir: &Path, mut header: Header) -> Result<Self> {
        let error = |source| Error::WriteRun {
            path: dir.to_owned(),
            source,
        };
        fs::create_dir_all(dir).map_err(error)?;
        let mut number = highest_run(dir).map_err(error)? + 1;
        // Creating the file claims the id; another window may have claimed it first.
        let (file, mut log) = loop {
            let file = dir.join(format!("R{number}.jsonl"));
            match File::options().append(true).create_new(true).open(&file) {
                Ok(log) => break (file, log),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => number += 1,
                Err(source) => return Err(Error::WriteRun { path: file, source }),
            }
        };
        header.id = format!("R{number}");
        log.write_all(&json_line(&header))
            .map_err(|source| Error::WriteRun {
                path: file.clone(),
                source,
            })?;
        Ok(Self {
            file,
            state: Mutex::new(RunState {
                header,
                entries: Vec::new(),
                permissions: Vec::new(),
                log: Some(log),
                cancel_requested: false,
                problem: None,
            }),
        })
    }

    /// The newest run log in `dir`, read as a finished run; `None` when there is none.
    fn latest(dir: &Path) -> Result<Option<Self>> {
        let error = |path: &Path, source| Error::ReadRun {
            path: path.to_owned(),
            source,
        };
        let number = match highest_run(dir) {
            Ok(0) => return Ok(None),
            Ok(number) => number,
            Err(source) => return Err(error(dir, source)),
        };
        let file = dir.join(format!("R{number}.jsonl"));
        let text = fs::read_to_string(&file).map_err(|source| error(&file, source))?;
        let mut lines = text.lines();
        let header: Header = lines
            .next()
            .and_then(|line| serde_json::from_str(line).ok())
            .ok_or_else(|| Error::InvalidRun {
                path: file.clone(),
                problem: "its first line is not a run header".to_owned(),
            })?;
        let mut entries = Vec::new();
        let mut problem = None;
        for (index, line) in lines.enumerate() {
            match serde_json::from_str(line) {
                Ok(entry) => entries.push(entry),
                // A torn last line, from a crash mid-write, or a newer build's entry kind: the
                // rest of the run is still worth showing.
                Err(error) => {
                    problem.get_or_insert_with(|| {
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
            file,
            state: Mutex::new(RunState {
                header,
                entries,
                permissions: Vec::new(),
                log: None,
                cancel_requested: false,
                problem,
            }),
        }))
    }

    fn state(&self) -> MutexGuard<'_, RunState> {
        lock(&self.state)
    }

    fn id(&self) -> String {
        self.state().header.id.clone()
    }

    /// Adds an entry to the run and its log. A run that ended takes no more entries.
    fn record(&self, entry: Entry) {
        let mut state = self.state();
        let Some(log) = &mut state.log else {
            return;
        };
        // One write per line, to a file opened for appending, so a crash leaves at most the
        // last line torn, which `latest` skips.
        let appended = log.write_all(&json_line(&entry));
        if matches!(entry, Entry::Ended { .. } | Entry::Failed { .. }) {
            state.log = None;
            // An open request can no longer be answered; the agent forgets it with the turn.
            state.permissions.iter_mut().for_each(|id| *id = None);
        }
        if let Err(error) = appended {
            state.problem.get_or_insert_with(|| {
                format!(
                    "the run log {} could not be written, so this run will not be shown after \
                     a restart: {error}",
                    self.file.display()
                )
            });
        }
        state.entries.push(entry);
    }

    /// Records an agent's event.
    fn record_event(&self, event: Event, root: &Path) {
        let entry = match event {
            Event::Update(update) => match entry_of(update, root) {
                Some(entry) => entry,
                None => return,
            },
            Event::Permission(request) => {
                let mut state = self.state();
                if !state.is_live() {
                    return;
                }
                let number = state.permissions.len();
                state.permissions.push(Some(request.id));
                drop(state);
                Entry::Permission {
                    request: number,
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
                }
            }
            Event::TurnEnded(TurnEnd::Stopped(reason)) => Entry::Ended { reason },
            Event::TurnEnded(TurnEnd::Failed(error)) => Entry::Failed { error },
            Event::Unreadable(problem) => Entry::Unreadable { problem },
            Event::Exited { stderr } => Entry::Failed {
                error: Error::AgentExited { stderr }.to_string(),
            },
        };
        self.record(entry);
    }

    /// Marks the run cancelled and answers its open permission requests in the log (the
    /// agent's [`Agent::cancel`] answers them on the wire). Returns whether the agent has the
    /// prompt, and so needs to be told.
    fn request_cancel(&self) -> bool {
        let mut state = self.state();
        state.cancel_requested = true;
        let open: Vec<usize> = state
            .permissions
            .iter_mut()
            .enumerate()
            .filter_map(|(number, id)| id.take().map(|_| number))
            .collect();
        let started = state.entries.contains(&Entry::Started);
        drop(state);
        self.record(Entry::CancelRequested);
        for request in open {
            self.record(Entry::Answer {
                request,
                option: None,
            });
        }
        started
    }

    /// The agent's id of the open permission request `request`.
    fn open_permission(&self, request: usize) -> Result<RequestId> {
        self.state()
            .permissions
            .get(request)
            .cloned()
            .flatten()
            .ok_or(Error::NoSuchPermission)
    }

    /// Closes the permission request `request`, answered with `option`.
    fn answered(&self, request: usize, option: &str) {
        if let Some(id) = self.state().permissions.get_mut(request) {
            *id = None;
        }
        self.record(Entry::Answer {
            request,
            option: Some(option.to_owned()),
        });
    }

    fn set_problem(&self, problem: String) {
        self.state().problem.get_or_insert(problem);
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

impl RunState {
    fn is_live(&self) -> bool {
        self.log.is_some()
    }

    fn status(&self) -> Status {
        let last_end = self.entries.iter().rev().find_map(|entry| match entry {
            Entry::Ended { .. } => Some(Status::Ended),
            Entry::Failed { .. } => Some(Status::Failed),
            _ => None,
        });
        if let Some(status) = last_end {
            return status;
        }
        if !self.is_live() {
            return Status::Interrupted;
        }
        if !self.entries.contains(&Entry::Started) {
            return Status::Starting;
        }
        if self.cancel_requested {
            return Status::Cancelling;
        }
        if self.permissions.iter().any(Option::is_some) {
            return Status::Waiting;
        }
        Status::Running
    }
}

/// The highest run number among the logs in `dir`; 0 when there is none, or no `dir`.
fn highest_run(dir: &Path) -> io::Result<u64> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut highest = 0;
    for entry in entries {
        let name = entry?.file_name();
        let number = name
            .to_str()
            .and_then(|name| name.strip_prefix('R'))
            .and_then(|name| name.strip_suffix(".jsonl"))
            .and_then(|digits| digits.parse::<u64>().ok());
        highest = highest.max(number.unwrap_or(0));
    }
    Ok(highest)
}

/// A session update as a log entry; `None` for what the run panel does not show.
fn entry_of(update: SessionUpdate, root: &Path) -> Option<Entry> {
    let entry = match update {
        SessionUpdate::AgentMessageChunk {
            content: ContentBlock::Text { text },
        } => Entry::Message { text },
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

/// The prompt's text, in the format of docs/design.md, "The prompt". The quoted snippet
/// travels with each note because line numbers go stale the moment the agent edits a file.
fn prompt_text(notes: &[Note]) -> String {
    let mut text = String::from(
        "These are review notes an engineer wrote while reading this repository.\n\
         Address every note. Change nothing the notes do not ask for.\n\
         When you finish, list each note id with one line on what you did.\n",
    );
    for note in notes {
        let lines = if note.lines.start == note.lines.end {
            note.lines.start.to_string()
        } else {
            format!("{}-{}", note.lines.start, note.lines.end)
        };
        text.push_str(&format!("\n[{}] {}:{lines}\n", note.id, note.path));
        if note.anchor == Anchor::Lost {
            text.push_str("(These lines are no longer in the file; they were:)\n");
        }
        for line in note.snippet.split('\n') {
            text.push_str(&format!("> {line}\n"));
        }
        text.push_str(note.body.trim_end());
        text.push('\n');
    }
    text
}

/// The notes' files, each once, in note order.
fn files_of(notes: &[Note]) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    for note in notes {
        if !files.contains(&note.path) {
            files.push(note.path.clone());
        }
    }
    files
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
