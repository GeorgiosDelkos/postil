//! A running agent: its process, the threads around its pipes, and the requests in flight.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::acp::wire::{
    self, ClientCapabilities, ContentBlock, FsCapabilities, Incoming, InitializeParams,
    InitializeResult, NewSessionParams, NewSessionResult, Notification, Outcome, PermissionOption,
    PermissionOutcome, PermissionParams, PermissionResult, PromptParams, PromptResult, Request,
    Response, RpcError, SessionParams, SessionUpdate, StopReason, ToolCallUpdate, UpdateParams,
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
    /// How long each start-up request (`initialize`, `session/new`) may take.
    pub(crate) timeout: Duration,
}

/// What the agent reports, in the order it reported it. Delivered on the agent's reader
/// thread.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// The session changed: a message, a thought, a tool call, the plan.
    Update(SessionUpdate),
    /// The agent asks to run a tool call; answer with [`Agent::answer_permission`].
    Permission(PermissionRequest),
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PermissionRequest {
    pub(crate) id: RequestId,
    pub(crate) tool_call: ToolCallUpdate,
    pub(crate) options: Vec<PermissionOption>,
}

/// The id of a request the agent sent: a number or a string, chosen by the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RequestId(Value);

/// How a turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TurnEnd {
    Stopped(StopReason),
    /// The agent answered the prompt with an error, or with a reply the app cannot read.
    Failed(String),
}

/// A running agent with one session open in the repository.
///
/// Dropping it kills the agent's process group. Events already being delivered can still
/// arrive after the drop returns; none starts after it.
#[derive(Debug)]
pub(crate) struct Agent {
    process: Process,
    session: String,
}

impl Agent {
    /// Starts the agent, introduces the app (`initialize`), and opens a session in
    /// `launch.cwd` (`session/new`). Blocks until both are answered, up to `launch.timeout`
    /// each.
    ///
    /// `on_event` receives every [`Event`] from the start of the session on, on the agent's
    /// reader thread, so it must not block for long: the agent's messages wait behind it.
    ///
    /// # Errors
    ///
    /// [`Error::AgentNotFound`] or [`Error::StartAgent`] when the process cannot be started;
    /// [`Error::AgentSilent`] when a step is not answered in time (an agent that waits for a
    /// sign-in in a browser does this); [`Error::AgentExited`] when the process ends first;
    /// [`Error::AgentRefused`] when the agent answers with an error;
    /// [`Error::AgentProtocol`] when it speaks another version or replies in a shape the app
    /// cannot read.
    pub(crate) fn start(
        launch: &Launch<'_>,
        on_event: impl FnMut(Event) + Send + 'static,
    ) -> Result<Self> {
        let process = Process::spawn(launch, on_event)?;

        let initialize = InitializeParams {
            protocol_version: wire::PROTOCOL_VERSION,
            client_capabilities: ClientCapabilities {
                fs: FsCapabilities {
                    read_text_file: false,
                    write_text_file: false,
                },
                terminal: false,
            },
        };
        let reply: InitializeResult = process.call("initialize", &initialize, launch.timeout)?;
        // The agent answers with the version it will speak; another version is one this
        // client cannot follow.
        if reply.protocol_version != wire::PROTOCOL_VERSION {
            return Err(Error::AgentProtocol(format!(
                "the agent speaks ACP version {}, and Postil speaks version {}",
                reply.protocol_version,
                wire::PROTOCOL_VERSION
            )));
        }

        let cwd = launch.cwd.to_str().ok_or_else(|| {
            Error::AgentProtocol(format!(
                "the repository path {} is not UTF-8, which ACP cannot carry",
                launch.cwd.display()
            ))
        })?;
        let session = NewSessionParams {
            cwd,
            mcp_servers: [],
        };
        let reply: NewSessionResult = process.call("session/new", &session, launch.timeout)?;
        Ok(Self {
            process,
            session: reply.session_id,
        })
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
            if state
                .waiters
                .values()
                .any(|waiter| matches!(waiter, Waiter::Turn))
            {
                return Err(Error::TurnRunning);
            }
            state.register(Waiter::Turn)
        };
        let id = id.ok_or_else(|| self.process.exited())?;
        let params = PromptParams {
            session_id: &self.session,
            prompt,
        };
        self.process.send(&request(id, "session/prompt", &params))
    }

    /// Asks the agent to stop the turn, and answers every open permission request
    /// `cancelled`, as the protocol requires. The turn still ends with
    /// [`Event::TurnEnded`], normally [`StopReason::Cancelled`].
    ///
    /// # Errors
    ///
    /// [`Error::AgentExited`] when the process is gone.
    pub(crate) fn cancel(&self) -> Result<()> {
        let open = std::mem::take(&mut self.process.state().permissions);
        let params = SessionParams {
            session_id: &self.session,
        };
        self.process
            .send(&notification("session/cancel", &params))?;
        for (id, _) in &open {
            let result = PermissionResult {
                outcome: PermissionOutcome::Cancelled,
            };
            self.process
                .send(&response(&id.0, Outcome::Result(result)))?;
        }
        Ok(())
    }

    /// Answers the permission request `id` with the option `option_id`, one the agent
    /// offered.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchPermission`] when no such request is open (it was answered, or
    /// cancelled); [`Error::BadRequest`] when the agent did not offer `option_id`, and the
    /// request stays open; [`Error::AgentExited`] when the process is gone.
    pub(crate) fn answer_permission(&self, id: &RequestId, option_id: &str) -> Result<()> {
        {
            let mut state = self.process.state();
            let index = state
                .permissions
                .iter()
                .position(|(open, _)| open == id)
                .ok_or(Error::NoSuchPermission)?;
            if !state.permissions[index]
                .1
                .iter()
                .any(|offered| offered == option_id)
            {
                return Err(Error::BadRequest(format!(
                    "the agent did not offer the answer {option_id:?}"
                )));
            }
            state.permissions.remove(index);
        }
        let result = PermissionResult {
            outcome: PermissionOutcome::Selected { option_id },
        };
        self.process.send(&response(&id.0, Outcome::Result(result)))
    }
}

/// The agent's process and what the threads around it share.
struct Process {
    child: Child,
    /// Lines for the writer thread to send. A channel rather than a locked `ChildStdin`, so a
    /// caller on the main thread never blocks on an agent that stopped reading.
    outbox: Sender<String>,
    shared: Arc<Shared>,
}

/// What the caller's thread and the reader thread share.
#[derive(Debug, Default)]
struct Shared {
    state: Mutex<State>,
    /// The end of standard error, written by the stderr thread.
    stderr: Mutex<Vec<u8>>,
}

/// The requests in flight in both directions. One lock: every change is a few map
/// operations, and nothing is called while it is held.
#[derive(Debug, Default)]
struct State {
    /// The id of the app's next request.
    next_id: u64,
    /// Who waits for the reply to each request the app sent.
    waiters: HashMap<u64, Waiter>,
    /// The agent's open permission requests, each with the option ids it offered.
    permissions: Vec<(RequestId, Vec<String>)>,
    /// Set when the [`Agent`] is dropped: the reader then delivers no more events.
    closed: bool,
    /// Set when the agent's output ended: every call fails from then on. Checked here, not
    /// left to the writer, which notices a closed pipe only on its next write.
    gone: bool,
}

/// Who waits for a reply.
#[derive(Debug)]
enum Waiter {
    /// A caller blocked in [`Process::call`].
    Call(SyncSender<std::result::Result<Value, RpcError>>),
    /// The prompt's reply, which ends the turn and becomes [`Event::TurnEnded`].
    Turn,
}

impl State {
    /// A new request id with `waiter` waiting for its reply; `None` once the agent is gone.
    fn register(&mut self, waiter: Waiter) -> Option<u64> {
        if self.gone {
            return None;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.waiters.insert(id, waiter);
        Some(id)
    }
}

/// Locks one of [`Shared`]'s mutexes. Poisoning is ignored: every change under these locks is
/// a single insert, remove, or assignment, so a panic cannot leave one half done.
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

        let started = spawn_thread("acp-writer", move || write_lines(stdin, &lines))
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
        let id = self
            .state()
            .register(Waiter::Call(sender))
            .ok_or_else(|| self.exited())?;
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
            match stdout.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            // Invalid UTF-8 cannot be JSON; the lossy text still names the line in the error.
            let text = String::from_utf8_lossy(&line);
            if !text.trim().is_empty() {
                self.handle(&text);
            }
        }

        // A timeout or a gone sender both mean: stop waiting for the agent's last words.
        stderr_closed.recv_timeout(STDERR_GRACE).ok();
        let closed = {
            let mut state = lock(&self.shared.state);
            state.gone = true;
            // Dropping the waiters wakes every blocked caller with "exited".
            state.waiters.clear();
            state.permissions.clear();
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
            (Some(id), None) => self.reply(&id, message.result, message.error),
            (Some(id), Some(method)) => self.request(id, &method, message.params),
            (None, Some(method)) => self.notification(&method, message.params),
            (None, None) => self.emit(Event::Unreadable(format!(
                "a message with neither an id nor a method: {}",
                excerpt(line)
            ))),
        }
    }

    /// A reply to one of the app's requests.
    fn reply(&mut self, id: &Value, result: Option<Value>, error: Option<RpcError>) {
        let waiter = id
            .as_u64()
            .and_then(|id| lock(&self.shared.state).waiters.remove(&id));
        let outcome = match (result, error) {
            (_, Some(error)) => Err(error),
            (Some(result), None) => Ok(result),
            // `"result": null` reads as no result; an `initialize` reply without one would
            // fail its own parse below, so this needs no case of its own.
            (None, None) => Ok(Value::Null),
        };
        match waiter {
            // The caller timed out and left: nobody wants the reply.
            Some(Waiter::Call(sender)) => drop(sender.send(outcome)),
            Some(Waiter::Turn) => {
                let end = match outcome {
                    Ok(result) => match serde_json::from_value::<PromptResult>(result) {
                        Ok(result) => TurnEnd::Stopped(result.stop_reason),
                        Err(error) => {
                            TurnEnd::Failed(format!("unreadable reply to the prompt: {error}"))
                        }
                    },
                    Err(error) => TurnEnd::Failed(format!("{} ({})", error.message, error.code)),
                };
                self.emit(Event::TurnEnded(end));
            }
            // A reply to a request that timed out, or to no request at all.
            None => {}
        }
    }

    /// A request from the agent. The app serves only permission requests; it advertised no
    /// file or terminal capability, so any other method is answered "not found".
    fn request(&mut self, id: Value, method: &str, params: Value) {
        if method != "session/request_permission" {
            let error = RpcError {
                code: wire::METHOD_NOT_FOUND,
                message: format!("Postil does not serve {method}"),
            };
            return self.answer(&id, error);
        }
        let params: PermissionParams = match serde_json::from_value(params) {
            Ok(params) => params,
            Err(error) => {
                let problem = format!("an unreadable permission request: {error}");
                let rpc = RpcError {
                    code: wire::INVALID_PARAMS,
                    message: problem.clone(),
                };
                self.answer(&id, rpc);
                return self.emit(Event::Unreadable(problem));
            }
        };
        let id = RequestId(id);
        let offered = params
            .options
            .iter()
            .map(|option| option.option_id.clone())
            .collect();
        lock(&self.shared.state)
            .permissions
            .push((id.clone(), offered));
        self.emit(Event::Permission(PermissionRequest {
            id,
            tool_call: params.tool_call,
            options: params.options,
        }));
    }

    fn notification(&mut self, method: &str, params: Value) {
        // Agents send notifications of their own (Grok's `_x.ai/...`, Claude's
        // `_auth/...`); only session updates change what the app shows.
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
fn write_lines(mut stdin: ChildStdin, lines: &Receiver<String>) {
    for line in lines {
        let written = stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.write_all(b"\n"))
            .and_then(|()| stdin.flush());
        if written.is_err() {
            break;
        }
    }
}

/// The stderr thread: keeps the last [`STDERR_TAIL`] bytes. Reading also keeps the agent
/// from blocking on a full pipe.
fn keep_tail(mut stderr: impl Read, tail: &Mutex<Vec<u8>>) {
    let mut buffer = [0; 4096];
    while let Ok(read @ 1..) = stderr.read(&mut buffer) {
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

    /// Long enough for Node to start on a loaded machine; a test that waits this long has
    /// failed (CON-20: it bounds a hang, it times nothing).
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
            timeout,
        };
        let agent = Agent::start(&launch, move |event| drop(sender.send(event)))?;
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

    #[test]
    fn a_turn_delivers_every_update_in_order_and_ends_with_the_stop_reason() {
        let (agent, events) = start("turn").unwrap();
        let link = ContentBlock::ResourceLink {
            uri: "file:///repo/src/a.rs".to_owned(),
            name: "src/a.rs".to_owned(),
        };
        agent.prompt(&[text("fix N1"), link]).unwrap();

        assert_eq!(said(&events), "got fix N1 | file:///repo/src/a.rs");
        assert_eq!(said(&events), "prompt 1");
        // The agent's own notifications and the usage update are not events; the file read
        // the app does not serve is refused with "method not found".
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
                    line: Some(3),
                }],
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
        agent.answer_permission(&request.id, "always").unwrap();
        assert_eq!(said(&events), "chose always");
        assert!(
            matches!(
                agent.answer_permission(&request.id, "once"),
                Err(Error::NoSuchPermission)
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
        assert_eq!(
            next(&events),
            Event::TurnEnded(TurnEnd::Stopped(StopReason::EndTurn))
        );

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
    fn an_answer_the_agent_did_not_offer_is_refused_and_the_request_stays_open() {
        let (agent, events) = start("turn").unwrap();
        agent.prompt(&[text("x")]).unwrap();
        let request = loop {
            if let Event::Permission(request) = next(&events) {
                break request;
            }
        };
        assert!(matches!(
            agent.answer_permission(&request.id, "yes please"),
            Err(Error::BadRequest(_))
        ));
        agent.answer_permission(&request.id, "no").unwrap();
        assert_eq!(said(&events), "chose no");
    }

    #[test]
    fn cancel_answers_open_permission_requests_cancelled_and_the_turn_ends_cancelled() {
        let (agent, events) = start("cancel").unwrap();
        agent.prompt(&[text("x")]).unwrap();
        assert_eq!(said(&events), "got x");
        said(&events);
        let Event::Permission(request) = next(&events) else {
            panic!("expected a permission request");
        };

        assert!(
            matches!(agent.prompt(&[text("y")]), Err(Error::TurnRunning)),
            "one turn at a time"
        );
        agent.cancel().unwrap();
        assert_eq!(said(&events), "permission cancelled");
        assert_eq!(
            next(&events),
            Event::TurnEnded(TurnEnd::Stopped(StopReason::Cancelled))
        );
        assert!(matches!(
            agent.answer_permission(&request.id, "once"),
            Err(Error::NoSuchPermission)
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
            Event::TurnEnded(TurnEnd::Stopped(StopReason::EndTurn))
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
            Event::TurnEnded(TurnEnd::Failed("model overloaded (-32603)".to_owned()))
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
        match agent.prompt(&[text("y")]) {
            Err(Error::AgentExited { stderr }) => assert_eq!(stderr, "panicked at the model"),
            other => panic!("expected AgentExited, got {other:?}"),
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

        match start("exit-on-start") {
            Err(Error::AgentExited { stderr }) => {
                assert_eq!(stderr, "Please sign in: run `fake login`");
            }
            other => panic!("expected AgentExited, got {other:?}"),
        }

        let auth = start("auth").unwrap_err();
        assert!(
            matches!(
                auth,
                Error::AgentRefused {
                    step: "session/new",
                    code: -32000,
                    ..
                }
            ),
            "{auth:?}"
        );
        assert!(auth.to_string().contains("needs you to sign in"), "{auth}");

        let version = start("version-2").unwrap_err().to_string();
        assert!(version.contains("speaks ACP version 2"), "{version}");

        let missing = start_command("no-such-agent-program --acp", WAIT).unwrap_err();
        assert!(
            matches!(&missing, Error::AgentNotFound { program } if program == "no-such-agent-program"),
            "{missing:?}"
        );
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
    fn long_lines_are_cut_on_a_character_boundary_in_messages() {
        let line = "é".repeat(300);
        let cut = excerpt(&line);
        assert_eq!(cut, format!("{}...", "é".repeat(200)));
        assert_eq!(excerpt("  short \n"), "short");
    }

    /// A real agent answers a prompt over a real session. Runs only when
    /// `POSTIL_REAL_AGENT` holds an agent's command (one of `agents::BUILT_IN`), because it
    /// needs the agent installed and signed in, and spends a turn of its model:
    /// `POSTIL_REAL_AGENT='grok agent stdio' cargo test real_agent -- --nocapture`.
    #[test]
    fn a_real_agent_answers_a_prompt() {
        let Ok(command) = std::env::var("POSTIL_REAL_AGENT") else {
            return;
        };
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
                }) => reply.push_str(&text),
                Event::TurnEnded(end) => break end,
                Event::Exited { stderr } => panic!("the agent exited: {stderr}"),
                Event::Update(_) | Event::Permission(_) | Event::Unreadable(_) => {}
            }
        };
        assert_eq!(end, TurnEnd::Stopped(StopReason::EndTurn), "{reply}");
        assert!(reply.to_lowercase().contains("pong"), "{reply}");
    }
}
