//! A running agent: its process, the threads around its pipes, and the requests in flight.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::acp::wire::{
    self, CancelParams, CancelRequestParams, ClientCapabilities, ContentBlock,
    ElicitationCapabilities, ElicitationParams, ElicitationReply, Empty, FsCapabilities, Incoming,
    InitializeParams, InitializeResult, McpServer, ModelConfig, ModelOption, NewSessionParams,
    NewSessionResult, Notification, Outcome, PermissionOption, PermissionOutcome, PermissionParams,
    PermissionResult, PromptParams, PromptResult, PromptUsage, Request, Response, RpcError,
    SessionUpdate, SetConfigOptionParams, SetConfigOptionResult, StopReason, ToolCallUpdate,
    UpdateParams, UsageCost,
};
use crate::agents;
use crate::error::{Error, Result};

/// How much of the agent's standard error is kept, from the end: enough for the last few
/// lines of a crash or a sign-in hint, which is what an error message shows. Arbitrary.
const STDERR_TAIL: usize = 4096;

/// How long the reader waits, once the agent's standard output closed, for standard error to
/// close too, so the error message holds the agent's last words. A grandchild that keeps
/// the pipe open would otherwise hold the reader forever. Arbitrary: a process that exits
/// closes both pipes within microseconds.
const STDERR_GRACE: Duration = Duration::from_secs(1);

/// The longest line of the agent's output that is read, in bytes, newline included; a longer
/// one is skipped and reported, so an agent that never ends a line cannot grow the app's
/// memory without bound. Arbitrary: far above a message that carries a source file's whole
/// diff, before and after. Tests use a smaller one, to cross it cheaply; the logic does not
/// depend on the value.
const LINE_LIMIT: usize = if cfg!(test) { 1 << 20 } else { 64 << 20 };

/// What starts an agent.
#[derive(Debug)]
pub(crate) struct Launch<'a> {
    /// The agent's command: a program and its arguments, split on whitespace
    /// (`settings.rs` refuses quotes).
    pub(crate) command: &'a str,
    /// The repository root, the session's working directory. Absolute.
    pub(crate) cwd: &'a Path,
    /// Where the program, and the programs it starts (`npx` runs `node`), are looked for.
    pub(crate) search_path: &'a OsStr,
    /// The MCP servers the session gets (`session/new`): hippius-mem, or none.
    pub(crate) mcp_servers: &'a [McpServer],
    /// How long each start-up request (`initialize`, `session/new`, and
    /// `session/set_config_option` when a model is set) may take.
    pub(crate) timeout: Duration,
}

/// What the agent reports, in the order it reported it. Delivered on the agent's reader
/// thread.
#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) enum Event {
    /// The session changed: a message, a thought, a tool call, the plan.
    Update(SessionUpdate),
    /// The agent asks to run a tool call; answer with [`Agent::answer_permission`].
    Permission(PermissionRequest),
    /// The agent asks the engineer a question; answer with [`Agent::answer_question`].
    Question(QuestionRequest),
    /// The agent withdrew the open request `number` (`$/cancel_request`): it is closed, and
    /// answered as a cancel, which ACP asks for all the same.
    Withdrawn { number: usize },
    /// The turn started by [`Agent::prompt`] ended.
    TurnEnded(TurnEnd),
    /// A message that could not be read; the text says what was wrong. The agent goes on.
    Unreadable(String),
    /// The agent's process ended, with the end of what it wrote to standard error. Nothing
    /// follows, and every call fails from now on.
    Exited {
        /// The last [`STDERR_TAIL`] bytes of standard error, lossily decoded.
        stderr: String,
    },
}

/// A permission request: the tool call the agent wants to make and the answers it offers.
#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub(crate) struct PermissionRequest {
    /// The request's number: 0 for the agent's first, counting on across turns. The agent's
    /// own request id stays inside [`Agent`], which keeps the only table of open requests.
    pub(crate) number: usize,
    pub(crate) tool_call: ToolCallUpdate,
    pub(crate) options: Vec<PermissionOption>,
}

/// A question: the agent's message and its form. The number counts with the permission
/// requests (`acp`, "Permission requests").
#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub(crate) struct QuestionRequest {
    pub(crate) number: usize,
    pub(crate) message: String,
    /// A JSON Schema object whose properties are the form's fields, as the agent sent it.
    pub(crate) schema: Value,
    /// The tool call the question belongs to, when the agent says.
    pub(crate) tool_call: Option<String>,
}

/// How a turn ended.
#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) enum TurnEnd {
    Stopped {
        reason: StopReason,
        usage: Option<PromptUsage>,
        cost: Option<UsageCost>,
    },
    /// The agent answered the prompt with an error, or with a reply the app cannot read.
    Failed(String),
}

/// An agent whose process runs and whose session is not open yet. Opening the session can
/// take minutes (an agent waiting for a browser sign-in never answers), so it is a step of
/// its own, which a [`Stopper`] can cut short from another thread.
#[derive(Debug)]
pub(crate) struct Starting {
    process: Process,
    cwd: PathBuf,
    mcp_servers: Vec<McpServer>,
    timeout: Duration,
}

/// An agent whose session [`Starting::open_session`] opened.
#[derive(Debug)]
pub(crate) struct Opened {
    pub(crate) agent: Agent,
    /// Why the session has none of the MCP servers the launch named: the agent refused
    /// `session/new` with them, and took it without. `None` when it took them, or when none
    /// were named.
    pub(crate) servers_refused: Option<String>,
}

/// Kills an agent's process group from another thread, while its process is not reaped yet.
/// A starting agent's [`Starting::open_session`] then fails with [`Error::AgentExited`]; the
/// writer uses one to end an agent it can no longer write to. Stopping an agent whose process
/// was reaped does nothing.
#[derive(Debug)]
pub(crate) struct Stopper {
    group: Arc<Mutex<Option<u32>>>,
}

impl Stopper {
    pub(crate) fn stop(&self) {
        // Signalled under the lock that [`Process`]'s drop takes before it reaps the process,
        // so the group id cannot have been reused by then.
        if let Some(leader) = *lock(&self.group) {
            agents::kill_group(leader);
        }
    }
}

/// A running agent with one session open in the repository.
///
/// Dropping it kills the agent's process group. Events already being delivered can still
/// arrive after the drop returns; none starts after it.
#[derive(Debug)]
pub(crate) struct Agent {
    process: Process,
    session: String,
    /// The model in effect: the one [`Starting::open_session`] set, or the one the agent
    /// reported in `session/new`, when it sent one.
    model: Option<String>,
    /// The models the agent's model select offered in `session/new`, in its order; empty when
    /// it has none.
    offered: Vec<ModelOption>,
}

impl Agent {
    /// Starts the agent's process. Its session is opened by [`Starting::open_session`].
    ///
    /// `on_event` receives every [`Event`] from the start of the session on, on the agent's
    /// reader thread, so it must not block for long: the agent's messages wait behind it.
    ///
    /// # Errors
    ///
    /// [`Error::AgentNotFound`] or [`Error::StartAgent`] when the process cannot be started.
    pub(crate) fn spawn(
        launch: &Launch<'_>,
        on_event: impl FnMut(Event) + Send + 'static,
    ) -> Result<Starting> {
        Ok(Starting {
            process: Process::spawn(launch, on_event)?,
            cwd: launch.cwd.to_owned(),
            mcp_servers: launch.mcp_servers.to_vec(),
            timeout: launch.timeout,
        })
    }
}

impl Starting {
    /// The handle that stops this agent from another thread.
    pub(crate) fn stopper(&self) -> Stopper {
        Stopper {
            group: Arc::clone(&self.process.group),
        }
    }

    /// Introduces the app (`initialize`) and opens a session in the repository
    /// (`session/new`) with the launch's MCP servers, then, when `model` is given, sets the
    /// session to it (`session/set_config_option`), so no prompt can reach the agent before its
    /// model is set. Blocks until each step is answered, up to the launch's timeout each. An
    /// agent that refuses the servers is asked once more without them ([`new_session`]).
    ///
    /// The model is checked against the ones the agent offers first: an agent may take an id
    /// it does not know and fall back to another model without a word.
    ///
    /// # Errors
    ///
    /// [`Error::AgentSilent`] when a step is not answered in time (an agent that waits for a
    /// sign-in in a browser does this); [`Error::AgentExited`] when the process ends first,
    /// or is stopped; [`Error::AgentRefused`] when the agent answers with an error (a second
    /// time, when it refused the servers); [`Error::AgentProtocol`] when it speaks another
    /// version or replies in a shape the app cannot read; [`Error::ModelNotChoosable`] when
    /// `model` is given and the agent offers no model choice, and [`Error::ModelNotOffered`]
    /// when it does not offer `model`.
    pub(crate) fn open_session(self, model: Option<&str>) -> Result<Opened> {
        let initialize = InitializeParams {
            protocol_version: wire::PROTOCOL_VERSION,
            client_capabilities: ClientCapabilities {
                fs: FsCapabilities {
                    read_text_file: false,
                    write_text_file: false,
                },
                terminal: false,
                elicitation: ElicitationCapabilities { form: Empty {} },
            },
        };
        let process = self.process;
        let reply: InitializeResult = process.call("initialize", &initialize, self.timeout)?;
        // The agent answers with the version it will speak; another version is one this
        // client cannot follow.
        if reply.protocol_version != wire::PROTOCOL_VERSION {
            return Err(Error::AgentProtocol(format!(
                "the agent speaks ACP version {}, and Postil speaks version {}",
                reply.protocol_version,
                wire::PROTOCOL_VERSION
            )));
        }

        let cwd = self.cwd.to_str().ok_or_else(|| {
            Error::AgentProtocol(format!(
                "the repository path {} is not UTF-8, which ACP cannot carry",
                self.cwd.display()
            ))
        })?;
        let NewSession {
            reply,
            servers_refused,
        } = new_session(&process, cwd, &self.mcp_servers, self.timeout)?;
        let model_config = reply.model_config();
        let model = match model {
            None => reply.current_model(),
            Some(wanted) => Some(set_model(
                &process,
                &reply.session_id,
                model_config.as_ref(),
                wanted,
                self.timeout,
            )?),
        };
        let agent = Agent {
            process,
            session: reply.session_id,
            model,
            offered: model_config.map_or_else(Vec::new, |config| config.options),
        };
        Ok(Opened {
            agent,
            servers_refused,
        })
    }
}

/// A session [`new_session`] opened: the agent's reply, and why the session has none of the
/// servers it was asked with, when it has none.
struct NewSession {
    reply: NewSessionResult,
    servers_refused: Option<String>,
}

/// Opens a session in `cwd` with `servers`. An agent may answer `session/new` with an error
/// (spike S7, docs/review-assist.md, "Open questions", 12): one that refuses the servers is
/// asked once more with none, so a run never fails for want of team memory. A refusal that
/// asks for a sign-in ([`wire::AUTH_REQUIRED`]) is not about the servers, and is not asked again.
fn new_session(
    process: &Process,
    cwd: &str,
    servers: &[McpServer],
    timeout: Duration,
) -> Result<NewSession> {
    let session = NewSessionParams {
        cwd,
        mcp_servers: servers,
    };
    let refused = match process.call("session/new", &session, timeout) {
        Ok(reply) => {
            return Ok(NewSession {
                reply,
                servers_refused: None,
            });
        }
        Err(Error::AgentRefused { code, message, .. })
            if !servers.is_empty() && code != wire::AUTH_REQUIRED =>
        {
            format!("the agent refused session/new with its MCP servers: {message} ({code})")
        }
        Err(error) => return Err(error),
    };
    let session = NewSessionParams {
        cwd,
        mcp_servers: &[],
    };
    Ok(NewSession {
        reply: process.call("session/new", &session, timeout)?,
        servers_refused: Some(refused),
    })
}

/// Sets session `session` to the model `wanted`, which `config`, the agent's model select,
/// must offer, and returns the model now in effect: the one the agent's reply reports, else
/// `wanted`.
fn set_model(
    process: &Process,
    session: &str,
    config: Option<&ModelConfig>,
    wanted: &str,
    timeout: Duration,
) -> Result<String> {
    let config = config.ok_or_else(|| Error::ModelNotChoosable {
        model: wanted.to_owned(),
    })?;
    if !config.offers(wanted) {
        return Err(Error::ModelNotOffered {
            model: wanted.to_owned(),
            offered: config
                .options
                .iter()
                .map(|option| option.value.clone())
                .collect(),
        });
    }
    let params = SetConfigOptionParams {
        session_id: session,
        config_id: &config.id,
        value: wanted,
    };
    let reply: SetConfigOptionResult =
        process.call("session/set_config_option", &params, timeout)?;
    let current = wire::model_config(&reply.config_options).and_then(|config| config.current);
    Ok(current.unwrap_or_else(|| wanted.to_owned()))
}

impl Agent {
    /// The active model id reported when the session opened, when known.
    pub(crate) fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// The models the agent offered when the session opened, in its order; empty when it
    /// offers no model choice.
    pub(crate) fn offered_models(&self) -> &[ModelOption] {
        &self.offered
    }

    /// Whether the agent is still there: false once the reader has seen its output end, or
    /// the writer could not write to it. A kept agent that is not alive is started again.
    pub(crate) fn is_alive(&self) -> bool {
        !self.process.state().gone
    }

    /// Sends a prompt and returns at once. The turn's progress arrives as [`Event`]s and its
    /// end as [`Event::TurnEnded`].
    ///
    /// # Errors
    ///
    /// [`Error::TurnRunning`] while the previous turn has not ended; [`Error::AgentExited`]
    /// when the process is gone.
    pub(crate) fn prompt(&self, prompt: &[ContentBlock]) -> Result<()> {
        let id = {
            let mut state = self.process.state();
            // The id first: `new_id` refuses a gone agent, whose turn may still be set until
            // the reader ends it. A refusal for a running turn then skips an id, which is
            // harmless: ids need only be unique.
            let Some(id) = state.new_id() else {
                drop(state);
                return Err(self.process.exited());
            };
            if state.turn.is_some() {
                return Err(Error::TurnRunning);
            }
            state.cancelling = false;
            state.turn = Some(id);
            id
        };
        let params = PromptParams {
            session_id: &self.session,
            prompt,
        };
        self.process.send(&request(id, "session/prompt", &params))
    }

    /// Asks the agent to stop the turn, and answers every open request `cancelled` (a
    /// permission request) or `cancel` (a question), as the protocol requires; so is every
    /// request that arrives until the turn ends. The turn still ends with
    /// [`Event::TurnEnded`], normally [`StopReason::Cancelled`]. Returns the numbers of the
    /// requests it answered, which may include one whose event ([`Event::Permission`] or
    /// [`Event::Question`]) is still on its way.
    ///
    /// # Errors
    ///
    /// [`Error::AgentExited`] when the process is gone.
    pub(crate) fn cancel(&self) -> Result<Vec<usize>> {
        let open = {
            let mut state = self.process.state();
            // Only while a turn runs: a cancel that comes just after the turn ended must not
            // make the next turn answer its requests `cancelled`.
            state.cancelling = state.turn.is_some();
            std::mem::take(&mut state.requests)
        };
        let params = CancelParams {
            session_id: &self.session,
        };
        self.process
            .send(&notification("session/cancel", &params))?;
        for open in open.values() {
            self.process.send(&cancelled(open))?;
        }
        Ok(open.into_keys().collect())
    }

    /// Answers the permission request numbered `number` with the option `option_id`, one the
    /// agent offered.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchRequest`] when no such request is open (it was answered, or
    /// cancelled); [`Error::BadRequest`] when the agent did not offer `option_id`, and the
    /// request stays open; [`Error::AgentExited`] when the process is gone.
    pub(crate) fn answer_permission(&self, number: usize, option_id: &str) -> Result<()> {
        let id = {
            let mut state = self.process.state();
            let Some(Open {
                kind: Kind::Permission { offered },
                ..
            }) = state.requests.get(&number)
            else {
                return Err(Error::NoSuchRequest);
            };
            if !offered.iter().any(|offered| offered == option_id) {
                return Err(Error::BadRequest(format!(
                    "the agent did not offer the answer {option_id:?}"
                )));
            }
            state
                .requests
                .remove(&number)
                .expect("checked to be open above")
                .id
        };
        let result = PermissionResult {
            outcome: PermissionOutcome::Selected { option_id },
        };
        self.process.send(&response(&id, Outcome::Result(result)))
    }

    /// Answers the question numbered `number` with `reply`: the form's values, or a skip.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchRequest`] when no such question is open (it was answered, withdrawn,
    /// or cancelled); [`Error::AgentExited`] when the process is gone.
    pub(crate) fn answer_question(&self, number: usize, reply: &ElicitationReply) -> Result<()> {
        let id = {
            let mut state = self.process.state();
            if !matches!(
                state.requests.get(&number),
                Some(Open {
                    kind: Kind::Question,
                    ..
                })
            ) {
                return Err(Error::NoSuchRequest);
            }
            state
                .requests
                .remove(&number)
                .expect("checked to be open above")
                .id
        };
        self.process.send(&response(&id, Outcome::Result(reply)))
    }
}

/// An open request of the agent's: its id, for the answer, and what kind of answer it takes.
#[derive(Debug)]
struct Open {
    id: Value,
    kind: Kind,
}

#[derive(Debug)]
enum Kind {
    /// A permission request, with the option ids the agent offered.
    Permission {
        offered: Vec<String>,
    },
    Question,
}

/// The answer that closes `open` because its turn is being cancelled.
fn cancelled(open: &Open) -> String {
    match open.kind {
        Kind::Permission { .. } => {
            let result = PermissionResult {
                outcome: PermissionOutcome::Cancelled,
            };
            response(&open.id, Outcome::Result(result))
        }
        Kind::Question => response(&open.id, Outcome::Result(ElicitationReply::Cancel)),
    }
}

/// The agent's process and what the threads around it share.
struct Process {
    child: Child,
    /// The process group's id while the process is not reaped, for [`Stopper`]; `None` from
    /// the moment the drop starts reaping it.
    group: Arc<Mutex<Option<u32>>>,
    /// Lines for the writer thread to send. A channel rather than a locked `ChildStdin`, so a
    /// caller on the request thread never blocks on an agent that stopped reading.
    outbox: Sender<String>,
    shared: Arc<Shared>,
}

/// What the caller's thread and the reader thread share.
#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    /// The end of standard error, written by the stderr thread.
    stderr: Mutex<Vec<u8>>,
}

// Sizes only: the agent's stderr can hold a sign-in URL, and the requests' contents are the
// agent's, so neither is printed.
impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared")
            .field("stderr_bytes", &lock(&self.stderr).len())
            .finish_non_exhaustive()
    }
}

/// The requests in flight in both directions. One lock: every change is a few map
/// operations, and nothing is called while it is held.
#[derive(Debug, Default)]
struct State {
    /// The id of the app's next request.
    next_id: u64,
    /// The callers blocked in [`Process::call`], by the id of the request each waits on.
    waiters: HashMap<u64, Caller>,
    /// The id of the prompt whose turn runs; its reply ends the turn and becomes
    /// [`Event::TurnEnded`]. One turn at a time: [`Agent::prompt`] refuses while it is set.
    turn: Option<u64>,
    /// The number the agent's next permission request or question gets.
    next_request: usize,
    /// The agent's permission requests and questions by number, while open: removed once
    /// answered, withdrawn, or closed with the turn. Ordered, so a cancel answers and reports
    /// them oldest first.
    requests: BTreeMap<usize, Open>,
    /// Set by [`Agent::cancel`] until the turn ends: requests that arrive meanwhile are
    /// answered `cancelled` at once.
    cancelling: bool,
    /// Set when the [`Agent`] is dropped: the reader then delivers no more events.
    closed: bool,
    /// Set when the agent's output ended, or a write to it failed: every call fails from then
    /// on. Checked here, not left to the writer, which notices a closed pipe only on its next
    /// write.
    gone: bool,
}

/// Where a blocked caller receives its reply.
type Caller = SyncSender<std::result::Result<Value, RpcError>>;

/// Whom a reply from the agent is for.
enum Addressee {
    /// A caller blocked in [`Process::call`].
    Caller(Caller),
    /// The turn, which the reply ends, with the requests that were still open.
    Turn(BTreeMap<usize, Open>),
}

impl State {
    /// A new id for a request of the app's; `None` once the agent is gone.
    fn new_id(&mut self) -> Option<u64> {
        if self.gone {
            return None;
        }
        let id = self.next_id;
        self.next_id += 1;
        Some(id)
    }

    /// A new request id with `caller` waiting for its reply; `None` once the agent is gone.
    fn register(&mut self, caller: Caller) -> Option<u64> {
        let id = self.new_id()?;
        self.waiters.insert(id, caller);
        Some(id)
    }
}

/// Locks one of [`Shared`]'s mutexes, or the process group's. Poisoning is ignored because
/// nothing panics while one is held: the code under [`Shared`]'s locks makes map operations
/// and assignments, calls nothing outside this file, and each `expect` there follows the check
/// that makes it hold; under the group's, [`Stopper::stop`] calls `agents::kill_group`, which
/// ignores every failure and has no panic path. A change can set several fields (the reader's
/// end clears the waiters, the turn and the requests), so the argument is that no panic
/// happens midway, not that a change is one step.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Process {
    fn spawn(launch: &Launch<'_>, on_event: impl FnMut(Event) + Send + 'static) -> Result<Self> {
        let mut words = launch.command.split_whitespace();
        let program = words
            .next()
            .ok_or_else(|| Error::BadRequest("the agent has no command".to_owned()))?;
        let path = agents::find_program(program, launch.search_path).ok_or_else(|| {
            Error::AgentNotFound {
                program: program.to_owned(),
            }
        })?;
        let mut child = Command::new(path)
            .args(words)
            .current_dir(launch.cwd)
            // The agent starts programs of its own by name (`npx` runs `node`), so it gets the
            // path its program was found on.
            .env("PATH", launch.search_path)
            // Its own process group, so dropping the agent ends what it started too
            // (`npx` leaves `node` running).
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| Error::StartAgent {
                command: launch.command.to_owned(),
                source,
            })?;

        let shared = Arc::new(Shared::default());
        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        let (outbox, lines) = mpsc::channel();
        let (stderr_done, stderr_closed) = mpsc::channel();
        let group = Arc::new(Mutex::new(Some(child.id())));

        let writer = Writer {
            shared: Arc::clone(&shared),
            stopper: Stopper {
                group: Arc::clone(&group),
            },
        };
        let started = spawn_thread("acp-writer", move || writer.run(stdin, &lines))
            .and_then(|()| {
                let shared = Arc::clone(&shared);
                spawn_thread("acp-stderr", move || {
                    keep_tail(stderr, &shared.stderr);
                    // The reader may have given up waiting; nobody listens then.
                    stderr_done.send(()).ok();
                })
            })
            .and_then(|()| {
                let reader = Reader {
                    shared: Arc::clone(&shared),
                    outbox: outbox.clone(),
                    on_event,
                };
                spawn_thread("acp-reader", move || reader.run(stdout, &stderr_closed))
            });
        let process = Self {
            child,
            group,
            outbox,
            shared,
        };
        // On failure `process` is dropped here, which kills the agent.
        started
            .map(|()| process)
            .map_err(|source| Error::StartAgent {
                command: launch.command.to_owned(),
                source,
            })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.shared.state)
    }

    /// Sends a request and waits up to `timeout` for its reply.
    fn call<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &'static str,
        params: &P,
        timeout: Duration,
    ) -> Result<R> {
        let (sender, receiver) = mpsc::sync_channel(1);
        let id = self.state().register(sender).ok_or_else(|| self.exited())?;
        self.send(&request(id, method, params))?;
        match receiver.recv_timeout(timeout) {
            Ok(Ok(result)) => serde_json::from_value(result).map_err(|error| {
                Error::AgentProtocol(format!("unreadable reply to {method}: {error}"))
            }),
            Ok(Err(error)) => Err(Error::AgentRefused {
                step: method,
                code: error.code,
                message: error.message,
            }),
            Err(RecvTimeoutError::Timeout) => {
                self.state().waiters.remove(&id);
                Err(Error::AgentSilent {
                    step: method,
                    seconds: timeout.as_secs(),
                })
            }
            // The reader dropped every waiter: the agent exited.
            Err(RecvTimeoutError::Disconnected) => Err(self.exited()),
        }
    }

    /// Queues one message for the writer thread.
    fn send(&self, line: &str) -> Result<()> {
        if self.state().gone {
            return Err(self.exited());
        }
        self.outbox.send(line.to_owned()).map_err(|_| self.exited())
    }

    fn exited(&self) -> Error {
        Error::AgentExited {
            stderr: tail_text(&lock(&self.shared.stderr)),
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.state().closed = true;
        // From here no [`Stopper`] signals the group: the process is about to be reaped.
        lock(&self.group).take();
        // This blocks the caller (the runner drops agents after releasing its lock, on the request,
        // start or reader thread) only as long as `/bin/kill` runs and the killed process takes
        // to be reaped: milliseconds, since SIGKILL cannot be caught.
        agents::kill_group(self.child.id());
        // Fails when the group's kill already took the agent, or it had exited: both fine.
        drop(self.child.kill());
        drop(self.child.wait());
    }
}

impl std::fmt::Debug for Process {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Process")
            .field("pid", &self.child.id())
            .finish_non_exhaustive()
    }
}

/// The reader thread: reads the agent's standard output line by line and routes each
/// message.
struct Reader<F> {
    shared: Arc<Shared>,
    /// For replies to the agent's requests.
    outbox: Sender<String>,
    on_event: F,
}

impl<F: FnMut(Event)> Reader<F> {
    fn run(mut self, stdout: impl Read, stderr_closed: &Receiver<()>) {
        let mut stdout = BufReader::new(stdout);
        let mut line = Vec::new();
        loop {
            line.clear();
            // `read_until` and `skip_until` retry a read that a signal interrupted, so an error
            // here is a real one, and ends the agent's output as its end does.
            match (&mut stdout)
                .take(LINE_LIMIT as u64)
                .read_until(b'\n', &mut line)
            {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            // Cut off at the limit before its newline. A line of exactly the limit at the end
            // of the output, with no newline, reads as too long too: no agent ends that way.
            if line.len() == LINE_LIMIT && line.last() != Some(&b'\n') {
                let start = String::from_utf8_lossy(&line[..LINE_LIMIT.min(1024)]).into_owned();
                if stdout.skip_until(b'\n').is_err() {
                    break;
                }
                self.emit(Event::Unreadable(format!(
                    "a line longer than {LINE_LIMIT} bytes, skipped: {}",
                    excerpt(&start)
                )));
                continue;
            }
            // Invalid UTF-8 cannot be JSON; the lossy text still names the line in the error.
            let text = String::from_utf8_lossy(&line);
            if !text.trim().is_empty() {
                self.handle(&text);
            }
        }

        // Gone at once, so a caller that asks during the wait below starts a fresh agent
        // instead of sending to this one.
        lock(&self.shared.state).gone = true;
        // A timeout or a gone sender both mean: stop waiting for the agent's last words.
        stderr_closed.recv_timeout(STDERR_GRACE).ok();
        let closed = {
            let mut state = lock(&self.shared.state);
            // Dropped after the wait, so a blocked caller's "exited" error carries the agent's
            // last words. Dropping the waiters wakes every blocked caller with "exited".
            state.waiters.clear();
            // No reply ends the turn now, and a prompt must fail as "exited", not as a turn
            // still running.
            state.turn = None;
            state.requests.clear();
            state.closed
        };
        if !closed {
            let stderr = tail_text(&lock(&self.shared.stderr));
            (self.on_event)(Event::Exited { stderr });
        }
    }

    fn handle(&mut self, line: &str) {
        let message: Incoming = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(error) => return self.emit(Event::Unreadable(unreadable(line, &error))),
        };
        match (message.id, message.method) {
            (Some(id), None) => {
                let error = message.error.as_ref().map(RpcError::from_value);
                self.reply(&id, message.result, error);
            }
            (Some(id), Some(method)) => self.request(&id, &method, message.params),
            (None, Some(method)) => self.notification(&method, message.params),
            (None, None) => self.emit(Event::Unreadable(format!(
                "a message with neither an id nor a method: {}",
                excerpt(line)
            ))),
        }
    }

    /// A reply to one of the app's requests.
    fn reply(&mut self, id: &Value, result: Option<Value>, error: Option<RpcError>) {
        let addressee = id.as_u64().and_then(|id| {
            let mut state = lock(&self.shared.state);
            if state.turn == Some(id) {
                state.turn = None;
                state.cancelling = false;
                Some(Addressee::Turn(std::mem::take(&mut state.requests)))
            } else {
                state.waiters.remove(&id).map(Addressee::Caller)
            }
        });
        let outcome = match (result, error) {
            (_, Some(error)) => Err(error),
            (Some(result), None) => Ok(result),
            // `"result": null` reads as no result; an `initialize` reply without one would
            // fail its own parse below, so this needs no case of its own.
            (None, None) => Ok(Value::Null),
        };
        match addressee {
            // The caller timed out and left: nobody wants the reply.
            Some(Addressee::Caller(sender)) => drop(sender.send(outcome)),
            Some(Addressee::Turn(open)) => {
                // A request still open when its turn ended is answered as a cancel, which
                // ACP asks of a request its sender gave up on.
                for open in open.values() {
                    drop(self.outbox.send(cancelled(open)));
                }
                let end = match outcome {
                    Ok(result) => match serde_json::from_value::<PromptResult>(result) {
                        Ok(result) => TurnEnd::Stopped {
                            reason: result.stop_reason,
                            usage: result.token_usage(),
                            cost: result.turn_cost(),
                        },
                        Err(error) => {
                            TurnEnd::Failed(format!("unreadable reply to the prompt: {error}"))
                        }
                    },
                    // Worded as a refused start is, so a sign-in error keeps its hint.
                    Err(error) => TurnEnd::Failed(
                        Error::AgentRefused {
                            step: "session/prompt",
                            code: error.code,
                            message: error.message,
                        }
                        .to_string(),
                    ),
                };
                self.emit(Event::TurnEnded(end));
            }
            // A reply to a request that timed out, or to no request at all.
            None => {}
        }
    }

    /// A request from the agent. The app serves permission requests and form questions; it
    /// advertised no file or terminal capability, so any other method is answered "not found".
    fn request(&mut self, id: &Value, method: &str, params: Value) {
        match method {
            "session/request_permission" => self.permission(id, params),
            // The name in ACP's unstable schema at the pinned commit, and the one Claude's
            // adapter sends, from a later SDK (`wire::ElicitationParams`).
            "elicitation/create" | "session/create_elicitation" => self.question(id, params),
            _ => {
                let error = RpcError {
                    code: wire::METHOD_NOT_FOUND,
                    message: format!("Postil does not serve {method}"),
                };
                self.answer(id, error);
            }
        }
    }

    fn permission(&mut self, id: &Value, params: Value) {
        let params: PermissionParams = match serde_json::from_value(params) {
            Ok(params) => params,
            Err(error) => {
                return self.refuse(id, format!("an unreadable permission request: {error}"));
            }
        };
        let offered = params
            .options
            .iter()
            .map(|option| option.option_id.clone())
            .collect();
        let open = Open {
            id: id.clone(),
            kind: Kind::Permission { offered },
        };
        let Some(number) = self.open(open) else {
            return;
        };
        self.emit(Event::Permission(PermissionRequest {
            number,
            tool_call: params.tool_call,
            options: params.options,
        }));
    }

    /// A question as a form. A URL question, or a form with no fields, is refused as one the
    /// app cannot show: the agent then goes on without an answer.
    fn question(&mut self, id: &Value, params: Value) {
        let params: ElicitationParams = match serde_json::from_value(params) {
            Ok(params) => params,
            Err(error) => return self.refuse(id, format!("an unreadable question: {error}")),
        };
        let has_fields = |schema: &Value| {
            schema["properties"]
                .as_object()
                .is_some_and(|fields| !fields.is_empty())
        };
        let schema = match (params.mode.as_deref(), params.requested_schema) {
            (None | Some("form"), Some(schema)) if has_fields(&schema) => schema,
            (Some("url"), _) => {
                return self.refuse(id, "a URL question, which Postil cannot show".to_owned());
            }
            (mode, _) => {
                let mode = mode.unwrap_or("form");
                return self.refuse(id, format!("a {mode} question with no form fields"));
            }
        };
        let open = Open {
            id: id.clone(),
            kind: Kind::Question,
        };
        let Some(number) = self.open(open) else {
            return;
        };
        self.emit(Event::Question(QuestionRequest {
            number,
            message: params.message,
            schema,
            tool_call: params.tool_call_id,
        }));
    }

    /// Numbers `open` and keeps it until it is answered; `None`, and answered at once, while
    /// the turn is being cancelled: the request crossed the cancel on the wire.
    fn open(&mut self, open: Open) -> Option<usize> {
        let mut state = lock(&self.shared.state);
        if state.cancelling {
            drop(state);
            self.outbox.send(cancelled(&open)).ok();
            return None;
        }
        let number = state.next_request;
        state.next_request += 1;
        state.requests.insert(number, open);
        Some(number)
    }

    /// Answers a request the app cannot read or serve with an error, and says so in the run.
    fn refuse(&mut self, id: &Value, problem: String) {
        let rpc = RpcError {
            code: wire::INVALID_PARAMS,
            message: problem.clone(),
        };
        self.answer(id, rpc);
        self.emit(Event::Unreadable(problem));
    }

    fn notification(&mut self, method: &str, params: Value) {
        // Agents send notifications of their own (Grok's `_x.ai/...`, Claude's
        // `_auth/...`); only session updates and withdrawn requests change what the app shows.
        if method == "$/cancel_request" {
            return self.withdraw(params);
        }
        if method != "session/update" {
            return;
        }
        match serde_json::from_value::<UpdateParams>(params) {
            Ok(UpdateParams {
                update: SessionUpdate::Other,
            }) => {}
            Ok(params) => self.emit(Event::Update(params.update)),
            Err(error) => self.emit(Event::Unreadable(format!(
                "an unreadable session update: {error}"
            ))),
        }
    }

    /// Closes the open request the agent withdrew, and answers it as a cancel: ACP's
    /// cancellation rule asks for a reply to the original request all the same. One already
    /// answered, or not the app's to answer, is ignored.
    fn withdraw(&mut self, params: Value) {
        let request_id = match serde_json::from_value::<CancelRequestParams>(params) {
            Ok(params) => params.request_id,
            Err(error) => {
                let problem = format!("an unreadable $/cancel_request: {error}");
                return self.emit(Event::Unreadable(problem));
            }
        };
        let closed = {
            let mut state = lock(&self.shared.state);
            let found = state
                .requests
                .iter()
                .find_map(|(number, open)| (open.id == request_id).then_some(*number));
            found.and_then(|number| state.requests.remove_entry(&number))
        };
        if let Some((number, open)) = closed {
            drop(self.outbox.send(cancelled(&open)));
            self.emit(Event::Withdrawn { number });
        }
    }

    fn answer(&self, id: &Value, error: RpcError) {
        let line = response::<()>(id, Outcome::Error(error));
        // Fails only once the writer is gone, when the agent has exited and the reader is
        // about to see the end of its output.
        drop(self.outbox.send(line));
    }

    /// Delivers an event, unless the agent was dropped.
    fn emit(&mut self, event: Event) {
        if !lock(&self.shared.state).closed {
            (self.on_event)(event);
        }
    }
}

/// The writer thread: sends each queued line, until the agent stops reading or the
/// [`Process`] is dropped.
#[derive(Debug)]
struct Writer {
    shared: Arc<Shared>,
    /// Ends the agent when a write fails.
    stopper: Stopper,
}

impl Writer {
    fn run(self, mut stdin: ChildStdin, lines: &Receiver<String>) {
        for line in lines {
            let written = stdin
                .write_all(line.as_bytes())
                .and_then(|()| stdin.write_all(b"\n"))
                .and_then(|()| stdin.flush());
            if written.is_err() {
                return self.give_up();
            }
        }
    }

    /// The agent stopped reading, and may run on: no prompt, answer or cancel can reach it
    /// any more. It is marked gone at once, so it is not alive and a prompt, or any message
    /// sent, fails as exited; then it is ended, so its output ends and the reader reports the
    /// exit, which also ends a turn that waited on a reply the agent never saw. No test
    /// covers the early mark alone: the reader marks the agent gone too once its output
    /// ends, milliseconds later.
    fn give_up(&self) {
        lock(&self.shared.state).gone = true;
        self.stopper.stop();
    }
}

/// The stderr thread: keeps the last [`STDERR_TAIL`] bytes. Reading also keeps the agent
/// from blocking on a full pipe.
fn keep_tail(mut stderr: impl Read, tail: &Mutex<Vec<u8>>) {
    let mut buffer = [0; 4096];
    loop {
        let read = match stderr.read(&mut buffer) {
            Ok(0) => return,
            Ok(read) => read,
            // A signal cut the read short: stopping here would leave the pipe unread, and an
            // agent that fills it blocks.
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        let mut tail = lock(tail);
        tail.extend_from_slice(&buffer[..read]);
        let excess = tail.len().saturating_sub(STDERR_TAIL);
        tail.drain(..excess);
    }
}

fn tail_text(tail: &[u8]) -> String {
    String::from_utf8_lossy(tail).trim().to_owned()
}

fn spawn_thread(name: &str, run: impl FnOnce() + Send + 'static) -> std::io::Result<()> {
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(run)
        .map(drop)
}

fn request<P: Serialize>(id: u64, method: &str, params: &P) -> String {
    let request = Request {
        jsonrpc: "2.0",
        id,
        method,
        params,
    };
    serde_json::to_string(&request).expect("the app's own messages serialize")
}

fn notification<P: Serialize>(method: &str, params: &P) -> String {
    let notification = Notification {
        jsonrpc: "2.0",
        method,
        params,
    };
    serde_json::to_string(&notification).expect("the app's own messages serialize")
}

fn response<R: Serialize>(id: &Value, outcome: Outcome<R>) -> String {
    let response = Response {
        jsonrpc: "2.0",
        id,
        outcome,
    };
    serde_json::to_string(&response).expect("the app's own messages serialize")
}

fn unreadable(line: &str, error: &serde_json::Error) -> String {
    format!("a line that is not JSON-RPC ({error}): {}", excerpt(line))
}

/// The start of a line, for an error message: an agent's line can be megabytes.
fn excerpt(line: &str) -> String {
    const LIMIT: usize = 200;
    let line = line.trim();
    match line.char_indices().nth(LIMIT) {
        Some((end, _)) => format!("{}...", &line[..end]),
        None => line.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::mpsc::Receiver;

    use super::*;
    use crate::acp::wire::{
        Location, PermissionKind, PlanEntry, PlanEntryStatus, ToolCall, ToolCallContent,
        ToolCallStatus, ToolKind,
    };
    use crate::testing::{TempDir, make_fifo, read_fifo};

    /// Long enough for Node to start on a loaded machine; a test that waits this long has
    /// failed (it bounds a hang, it times nothing).
    const WAIT: Duration = Duration::from_secs(20);

    fn fake_agent() -> PathBuf {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake-agent.mjs");
        assert!(
            !script.to_string_lossy().contains(char::is_whitespace),
            "agent commands are split on whitespace, so the checkout path cannot hold any"
        );
        script
    }

    fn command(scenario: &str) -> String {
        format!("node {} {scenario}", fake_agent().display())
    }

    /// Starts the fake agent in `scenario`, with its events on the returned channel.
    fn start(scenario: &str) -> Result<(Agent, Receiver<Event>)> {
        start_command(&command(scenario), WAIT)
    }

    fn start_command(command: &str, timeout: Duration) -> Result<(Agent, Receiver<Event>)> {
        let (sender, events) = mpsc::channel();
        let search_path = std::env::var_os("PATH").expect("PATH is set");
        let launch = Launch {
            command,
            cwd: Path::new(env!("CARGO_MANIFEST_DIR")),
            search_path: &search_path,
            mcp_servers: &[],
            timeout,
        };
        let agent = Agent::spawn(&launch, move |event| drop(sender.send(event)))?
            .open_session(None)?
            .agent;
        Ok((agent, events))
    }

    fn next(events: &Receiver<Event>) -> Event {
        events
            .recv_timeout(WAIT)
            .expect("the agent sends another event")
    }

    /// The text of the next event, which must be a message chunk.
    fn said(events: &Receiver<Event>) -> String {
        match next(events) {
            Event::Update(SessionUpdate::AgentMessageChunk {
                content: ContentBlock::Text { text },
                ..
            }) => text,
            other => panic!("expected a message, got {other:?}"),
        }
    }

    /// A path in the checkout, the fake agent's working directory, as agents report paths.
    fn in_checkout(path: &str) -> String {
        format!("{}/{path}", env!("CARGO_MANIFEST_DIR"))
    }

    fn text(text: &str) -> ContentBlock {
        ContentBlock::Text {
            text: text.to_owned(),
        }
    }

    fn assert_turn_usage(events: &Receiver<Event>) {
        assert!(matches!(
            next(events),
            Event::Update(SessionUpdate::UsageUpdate {
                used: Some(10),
                size: Some(100),
                cost: Some(_),
            })
        ));
    }

    fn assert_turn_ended_with_usage(events: &Receiver<Event>) {
        let Event::TurnEnded(TurnEnd::Stopped {
            reason: StopReason::EndTurn,
            usage: Some(usage),
            cost: None,
        }) = next(events)
        else {
            panic!("expected end_turn with usage");
        };
        assert_eq!(
            (usage.input_tokens, usage.output_tokens, usage.total_tokens),
            (Some(120), Some(30), Some(160))
        );
    }

    #[test]
    fn a_turn_delivers_every_update_in_order_and_ends_with_the_stop_reason() {
        let (agent, events) = start("turn").unwrap();
        assert_eq!(agent.model(), Some("m"));
        let link = ContentBlock::ResourceLink {
            uri: "file:///repo/src/a.rs".to_owned(),
            name: "src/a.rs".to_owned(),
        };
        agent.prompt(&[text("fix N1"), link]).unwrap();

        assert_eq!(said(&events), "got fix N1 | file:///repo/src/a.rs");
        assert_eq!(said(&events), "prompt 1");
        assert_turn_usage(&events);
        assert_eq!(said(&events), "fs answered -32601");
        assert_eq!(
            next(&events),
            Event::Update(SessionUpdate::AgentThoughtChunk {
                content: text("hmm")
            })
        );
        assert_eq!(
            next(&events),
            Event::Update(SessionUpdate::ToolCall(ToolCall {
                tool_call_id: "t1".to_owned(),
                title: "Edit src/a.rs".to_owned(),
                kind: ToolKind::Edit,
                status: ToolCallStatus::Pending,
                content: Vec::new(),
                locations: vec![Location {
                    path: in_checkout("src/a.rs"),
                    line: Some(3)
                }],
                raw_input: None,
                raw_output: None,
            }))
        );
        // A kind newer than the pinned schema reads as Other instead of losing the call.
        let Event::Update(SessionUpdate::ToolCall(call)) = next(&events) else {
            panic!("expected the second tool call");
        };
        assert_eq!(
            (call.tool_call_id.as_str(), call.kind),
            ("t2", ToolKind::Other)
        );

        let Event::Permission(request) = next(&events) else {
            panic!("expected a permission request");
        };
        assert_eq!(request.tool_call.tool_call_id, "t1");
        let kinds: Vec<_> = request.options.iter().map(|option| option.kind).collect();
        assert_eq!(
            kinds,
            [
                PermissionKind::AllowOnce,
                PermissionKind::AllowAlways,
                PermissionKind::RejectOnce
            ]
        );
        agent.answer_permission(request.number, "always").unwrap();
        assert_eq!(said(&events), "chose always");
        assert!(
            matches!(
                agent.answer_permission(request.number, "once"),
                Err(Error::NoSuchRequest)
            ),
            "a request is answered once"
        );

        let Event::Update(SessionUpdate::ToolCallUpdate(done)) = next(&events) else {
            panic!("expected the tool call's update");
        };
        assert_eq!(done.status, Some(ToolCallStatus::Completed));
        assert_eq!(
            done.content,
            Some(vec![ToolCallContent::Diff {
                path: in_checkout("src/a.rs"),
                old_text: Some("a".to_owned()),
                new_text: "b".to_owned(),
            }])
        );
        assert_eq!(done.title, None, "fields the update leaves out stay unset");
        assert_eq!(
            next(&events),
            Event::Update(SessionUpdate::Plan {
                entries: vec![PlanEntry {
                    content: "Fix N1".to_owned(),
                    status: PlanEntryStatus::Completed,
                }]
            })
        );
        assert_turn_ended_with_usage(&events);

        // The session stays open for the next prompt.
        agent.prompt(&[text("again")]).unwrap();
        assert_eq!(said(&events), "got again");
        assert_eq!(
            said(&events),
            "prompt 2",
            "the same process, so the same session"
        );
    }

    #[test]
    fn a_cancel_after_the_turn_ended_leaves_the_next_turn_s_requests_alone() {
        let (agent, events) = start("turn").unwrap();
        agent.prompt(&[text("x")]).unwrap();
        loop {
            match next(&events) {
                Event::Permission(request) => {
                    agent.answer_permission(request.number, "once").unwrap();
                }
                Event::TurnEnded(_) => break,
                _ => {}
            }
        }
        assert_eq!(agent.cancel().unwrap(), Vec::<usize>::new());

        agent.prompt(&[text("y")]).unwrap();
        let request = loop {
            if let Event::Permission(request) = next(&events) {
                break request;
            }
        };
        assert_eq!(request.number, 1, "numbers count on across turns");
        agent.answer_permission(request.number, "once").unwrap();
    }

    #[test]
    fn an_answer_the_agent_did_not_offer_is_refused_and_the_request_stays_open() {
        let (agent, events) = start("turn").unwrap();
        agent.prompt(&[text("x")]).unwrap();
        let request = loop {
            if let Event::Permission(request) = next(&events) {
                break request;
            }
        };
        assert!(matches!(
            agent.answer_permission(request.number, "yes please"),
            Err(Error::BadRequest(_))
        ));
        agent.answer_permission(request.number, "no").unwrap();
        assert_eq!(said(&events), "chose no");
    }

    #[test]
    fn cancel_answers_open_requests_cancelled_and_the_turn_ends_cancelled() {
        let (agent, events) = start("cancel").unwrap();
        agent.prompt(&[text("x")]).unwrap();
        assert_eq!(said(&events), "got x");
        said(&events);
        let Event::Question(question) = next(&events) else {
            panic!("expected a question");
        };

        assert!(
            matches!(agent.prompt(&[text("y")]), Err(Error::TurnRunning)),
            "one turn at a time"
        );
        assert_eq!(agent.cancel().unwrap(), [question.number]);
        assert_eq!(said(&events), "question cancel");
        // Asked while the cancel is under way: answered at once, and never an event.
        assert_eq!(said(&events), "late permission cancelled");
        assert_eq!(said(&events), "late question cancel");
        assert_eq!(
            next(&events),
            Event::TurnEnded(TurnEnd::Stopped {
                reason: StopReason::Cancelled,
                usage: None,
                cost: None,
            })
        );
        assert!(matches!(
            agent.answer_question(question.number, &ElicitationReply::Decline),
            Err(Error::NoSuchRequest)
        ));
    }

    #[test]
    fn unreadable_messages_are_reported_and_the_turn_goes_on() {
        let (agent, events) = start("garbage").unwrap();
        agent.prompt(&[text("x")]).unwrap();
        assert_eq!(said(&events), "got x");
        said(&events);
        for expected in [
            "a line that is not JSON-RPC",
            "a message with neither an id nor a method",
            "an unreadable session update",
            "an unreadable permission request",
        ] {
            match next(&events) {
                Event::Unreadable(problem) => assert!(problem.starts_with(expected), "{problem}"),
                other => panic!("expected {expected:?}, got {other:?}"),
            }
        }
        assert_eq!(said(&events), "bad permission answered -32602");
        assert_eq!(
            next(&events),
            Event::TurnEnded(TurnEnd::Stopped {
                reason: StopReason::EndTurn,
                usage: None,
                cost: None,
            })
        );
    }

    #[test]
    fn a_refused_prompt_ends_the_turn_with_the_agent_s_message() {
        let (agent, events) = start("refuse-prompt").unwrap();
        agent.prompt(&[text("x")]).unwrap();
        said(&events);
        said(&events);
        assert_eq!(
            next(&events),
            Event::TurnEnded(TurnEnd::Failed(
                "the agent refused session/prompt: model overloaded (-32603)".to_owned()
            ))
        );
    }

    #[test]
    fn an_error_reply_of_another_shape_still_ends_the_turn() {
        let (agent, events) = start("odd-error").unwrap();
        agent.prompt(&[text("x")]).unwrap();
        said(&events);
        said(&events);
        assert_eq!(
            next(&events),
            Event::TurnEnded(TurnEnd::Failed(
                "the agent refused session/prompt: no code here (0)".to_owned()
            ))
        );
    }

    #[test]
    fn an_agent_that_exits_mid_turn_reports_its_last_words_and_calls_fail() {
        let (agent, events) = start("exit-mid-turn").unwrap();
        agent.prompt(&[text("x")]).unwrap();
        said(&events);
        said(&events);
        assert_eq!(
            next(&events),
            Event::Exited {
                stderr: "panicked at the model".to_owned()
            }
        );
        // Twice: a prompt to a gone agent must not leave a turn behind that makes the next
        // one fail as "a turn is running".
        for attempt in ["y", "z"] {
            match agent.prompt(&[text(attempt)]) {
                Err(Error::AgentExited { stderr }) => assert_eq!(stderr, "panicked at the model"),
                other => panic!("{attempt}: expected AgentExited, got {other:?}"),
            }
        }
    }

    #[test]
    fn start_up_failures_name_the_step_and_the_fix() {
        let silent = start_command(&command("silent"), Duration::from_millis(300));
        assert!(
            matches!(
                silent,
                Err(Error::AgentSilent {
                    step: "initialize",
                    ..
                })
            ),
            "{silent:?}"
        );

        let unreadable = start("no-session-id").unwrap_err().to_string();
        assert!(
            unreadable.contains("unreadable reply to session/new"),
            "{unreadable}"
        );

        let version = start("version-2").unwrap_err().to_string();
        assert!(version.contains("speaks ACP version 2"), "{version}");
    }

    #[test]
    fn dropping_the_agent_ends_its_process_group_and_its_events() {
        let pid_file = std::env::temp_dir().join(format!("postil-acp-{}", std::process::id()));
        let command = format!("{} {}", command("spawns-child"), pid_file.display());
        let (agent, events) = start_command(&command, WAIT).unwrap();
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        drop(std::fs::remove_file(&pid_file));
        let alive = || {
            Command::new("/bin/kill")
                .args(["-0", &pid])
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        assert!(alive(), "the grandchild runs while the agent does");

        drop(agent);
        assert!(!alive(), "the grandchild went with the agent");
        // The reader ends without reporting an exit the app caused itself.
        assert!(matches!(
            events.recv_timeout(WAIT),
            Err(RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn only_the_end_of_standard_error_is_kept() {
        let tail = Mutex::new(Vec::new());
        let long = "x".repeat(STDERR_TAIL) + "the end";
        keep_tail(long.as_bytes(), &tail);
        let kept = tail.into_inner().unwrap();
        assert_eq!(kept.len(), STDERR_TAIL);
        assert!(kept.ends_with(b"the end"));
    }

    #[test]
    fn standard_error_is_read_on_after_an_interrupted_read_but_not_after_a_failed_one() {
        // A signal on a real pipe cannot be timed into a read, so the read is made to fail.
        let interrupted = std::io::Error::from(ErrorKind::Interrupted);
        let stderr = Read::chain(
            Read::chain(&b"before "[..], FailsOnce(Some(interrupted))),
            &b"after"[..],
        );
        let tail = Mutex::new(Vec::new());
        keep_tail(stderr, &tail);
        assert_eq!(tail.into_inner().unwrap(), b"before after");

        // Any other error ends the reading: retrying it could spin forever on a broken pipe.
        let broken = std::io::Error::from(ErrorKind::BrokenPipe);
        let stderr = Read::chain(
            Read::chain(&b"before "[..], FailsOnce(Some(broken))),
            &b"after"[..],
        );
        let tail = Mutex::new(Vec::new());
        keep_tail(stderr, &tail);
        assert_eq!(tail.into_inner().unwrap(), b"before ");
    }

    /// A reader whose first read fails with its error, and whose later reads end the input.
    struct FailsOnce(Option<std::io::Error>);

    impl Read for FailsOnce {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            self.0.take().map_or(Ok(0), Err)
        }
    }

    #[test]
    fn long_lines_are_cut_on_a_character_boundary_in_messages() {
        let line = "é".repeat(300);
        let cut = excerpt(&line);
        assert_eq!(cut, format!("{}...", "é".repeat(200)));
        assert_eq!(excerpt("  short \n"), "short");
    }

    #[test]
    fn a_starting_agent_can_be_stopped_from_another_thread() {
        let dir = TempDir::new("acp-silent");
        let pid_file = dir.path().join("silent.pid");
        make_fifo(&pid_file);
        let search_path = std::env::var_os("PATH").expect("PATH is set");
        let command = format!("{} {}", command("silent"), pid_file.display());
        let launch = Launch {
            command: &command,
            cwd: Path::new(env!("CARGO_MANIFEST_DIR")),
            search_path: &search_path,
            mcp_servers: &[],
            // Far longer than the test: only the stop can end the wait.
            timeout: Duration::from_secs(600),
        };
        let starting = Agent::spawn(&launch, |_| {}).unwrap();
        let stopper = starting.stopper();
        let opening = thread::spawn(move || starting.open_session(None));

        // The agent writes its pid once it runs; then it never answers.
        let pid = read_fifo(&pid_file, WAIT);
        stopper.stop();
        assert!(matches!(
            opening.join().unwrap(),
            Err(Error::AgentExited { .. })
        ));
        let alive = Command::new("/bin/kill")
            .args(["-0", &pid])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!alive, "the stopped agent is gone");
        stopper.stop();
    }

    /// JSON-RPC matches a reply to its request by id, so the ids of one connection must all
    /// differ, whatever order the app's requests come in.
    #[test]
    fn every_request_of_a_connection_gets_an_id_of_its_own() {
        let mut state = State::default();
        let (caller, _replies) = mpsc::sync_channel(1);
        let call = state.register(caller).unwrap();
        let turn = state.new_id().unwrap();
        let next = state.new_id().unwrap();
        assert!(
            call != turn && turn != next && call != next,
            "{call} {turn} {next}"
        );
    }

    #[test]
    fn an_agent_that_exited_is_not_alive() {
        let (agent, events) = start("exit-after-turn").unwrap();
        assert!(agent.is_alive());
        agent.prompt(&[text("x")]).unwrap();
        loop {
            if let Event::Exited { .. } = next(&events) {
                break;
            }
        }
        assert!(!agent.is_alive());
    }

    #[test]
    fn an_agent_that_closes_its_input_and_runs_on_is_ended_with_its_turn() {
        let (agent, events) = start("close-stdin").unwrap();
        // Queued at once; the writer's write then fails.
        agent.prompt(&[text("x")]).unwrap();
        // Nothing can reach the agent any more, so it is ended: its output ends, and so does
        // the turn, which no reply would ever end.
        assert!(matches!(next(&events), Event::Exited { .. }));
        assert!(!agent.is_alive(), "the runner starts a fresh agent");
        assert!(
            matches!(agent.prompt(&[text("y")]), Err(Error::AgentExited { .. })),
            "exited, not a turn still running"
        );
    }

    /// A real agent answers a prompt over a real session. Ignored unless asked for, with
    /// `POSTIL_REAL_AGENT` holding an agent's command (one of `agents::BUILT_IN`), because it
    /// needs the agent installed and signed in, and spends a turn of its model:
    /// `POSTIL_REAL_AGENT='grok agent stdio' cargo test real_agent -- --ignored`.
    #[test]
    #[ignore = "needs a signed-in agent's command in POSTIL_REAL_AGENT"]
    fn a_real_agent_answers_a_prompt() {
        let command = std::env::var("POSTIL_REAL_AGENT").expect("POSTIL_REAL_AGENT is set");
        let (agent, events) = start_command(&command, Duration::from_secs(120)).unwrap();
        agent
            .prompt(&[text(
                "Reply with the single word pong. Do not use any tools.",
            )])
            .unwrap();
        let mut reply = String::new();
        let end = loop {
            match events.recv_timeout(Duration::from_secs(300)).unwrap() {
                Event::Update(SessionUpdate::AgentMessageChunk {
                    content: ContentBlock::Text { text },
                    ..
                }) => reply.push_str(&text),
                Event::TurnEnded(end) => break end,
                Event::Exited { stderr } => panic!("the agent exited: {stderr}"),
                Event::Update(_)
                | Event::Permission(_)
                | Event::Question(_)
                | Event::Withdrawn { .. }
                | Event::Unreadable(_) => {}
            }
        };
        assert!(
            matches!(
                end,
                TurnEnd::Stopped {
                    reason: StopReason::EndTurn,
                    ..
                }
            ),
            "{end:?}: {reply}"
        );
        assert!(reply.to_lowercase().contains("pong"), "{reply}");
    }
}
