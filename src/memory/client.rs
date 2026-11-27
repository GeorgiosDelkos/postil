//! The MCP client of hippius-mem's stdio server: one [`Server`] process, newline-delimited
//! JSON-RPC 2.0 over its pipes (MCP 2025-06-18, as hippius-mem 0.7.1 speaks it, spike S3), and
//! the hand-written types of the tools Postil calls.
//!
//! One request is in flight at a time: [`Server::call_tool`] writes a request and reads until
//! the reply with its id, skipping any other. A request is small (a recall's text is cut at
//! [`super::RECALL_TEXT_LIMIT`]), and the server had read every earlier one before it answered
//! it, so writing one never blocks on a full pipe.

use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::de::{DeserializeOwned, IgnoredAny};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agents;

/// The tools Postil calls. A server whose `tools/list` lacks one does not match this Postil.
pub(super) const REQUIRED_TOOLS: [&str; 4] = ["get", "history", "recall", "refresh"];

/// The MCP version Postil speaks: hippius-mem 0.7.1's (spike S3, 2026-11-26).
const PROTOCOL_VERSION: &str = "2025-06-18";

/// The longest line read from the server, in bytes; a longer one is skipped. Arbitrary: far
/// above a recall's pointers or a 64 KiB note with its history.
const LINE_LIMIT: u64 = 16 << 20;

/// How much of the server's standard error is kept, in bytes: the end, which says why it
/// exited. Arbitrary: a few of its log lines.
const STDERR_TAIL: usize = 4096;

/// How long a server whose output ended may take to close its standard error, so its last
/// words come with the error. Arbitrary: a process that exits closes both at once.
const STDERR_GRACE: Duration = Duration::from_millis(500);

/// A running hippius-mem server, in its own process group. Dropping it kills the group and
/// reaps the process.
pub(super) struct Server {
    child: Child,
    stdin: ChildStdin,
    /// The replies the reader thread read, in order; closed once the server's output ended.
    replies: Receiver<Reply>,
    stderr: Arc<Mutex<Vec<u8>>>,
    next_id: u64,
}

/// Why a server's handshake did not succeed.
#[derive(Debug)]
pub(super) enum StartError {
    /// It did not finish the handshake.
    Failed(CallError),
    /// Its `tools/list` lacks `missing`; `server` is the name and version its `initialize`
    /// gave.
    Incompatible {
        missing: Vec<&'static str>,
        server: String,
    },
}

/// Why a call failed.
#[derive(Debug)]
pub(super) enum CallError {
    /// The server's output ended: it exited, or closed it. `stderr` is the end of what it
    /// printed there.
    Exited { stderr: String },
    /// No reply came by the deadline. The server may still answer: it must be stopped.
    TimedOut,
    /// The server answered with an error: its message, as it wrote it.
    Refused(String),
    /// The server answered with something else than the tool's result.
    Unreadable(String),
}

impl Server {
    /// Starts `command`, with threads reading its output. Waits for nothing: the handshake is
    /// [`Server::handshake`], so the caller can make the process stoppable first.
    pub(super) fn spawn(mut command: Command) -> io::Result<Self> {
        let mut child = command
            // Its own group, so stopping it ends what it started too.
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr_pipe = child.stderr.take().expect("stderr was piped");

        let stderr = Arc::new(Mutex::new(Vec::new()));
        // One reply is in flight at a time; a few more are stale ones, which a call skips.
        let (sender, replies) = mpsc::sync_channel(4);
        let (stderr_done, stderr_closed) = mpsc::sync_channel(1);
        let tail = Arc::clone(&stderr);
        let started = spawn_thread("memory-stderr", move || {
            keep_tail(stderr_pipe, &tail);
            // The reader may have stopped waiting; nobody listens then.
            stderr_done.send(()).ok();
        })
        .and_then(|()| {
            spawn_thread("memory-stdout", move || {
                read_replies(stdout, &sender);
                // The output ended: wait for the last words before the receiver sees the end.
                stderr_closed.recv_timeout(STDERR_GRACE).ok();
            })
        });
        let server = Self {
            child,
            stdin,
            replies,
            stderr,
            next_id: 1,
        };
        // On failure `server` is dropped here, which stops the process.
        started.map(|()| server)
    }

    /// The process id, which is also its group's id while it runs.
    pub(super) fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Runs the MCP handshake (`initialize`, `notifications/initialized`, `tools/list`) by
    /// `deadline`, and checks that the server offers every tool in [`REQUIRED_TOOLS`]. Does not
    /// wait past the deadline.
    pub(super) fn handshake(&mut self, deadline: Instant) -> Result<(), StartError> {
        let params = serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "postil", "version": env!("CARGO_PKG_VERSION") },
        });
        let initialized: Initialized = self
            .request("initialize", &params, deadline)
            .and_then(read)
            .map_err(StartError::Failed)?;
        self.notify("notifications/initialized");
        let tools: ToolList = self
            .request("tools/list", &serde_json::json!({}), deadline)
            .and_then(read)
            .map_err(StartError::Failed)?;
        let missing: Vec<&'static str> = REQUIRED_TOOLS
            .into_iter()
            .filter(|name| !tools.tools.iter().any(|tool| tool.name == *name))
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        let info = initialized.server_info;
        Err(StartError::Incompatible {
            missing,
            server: format!("{} {}", info.name, info.version),
        })
    }

    /// Calls the tool `name` with `arguments` and reads its result, the JSON in its one text
    /// content, as a `T`. Waits for the reply until `deadline`, no longer.
    pub(super) fn call_tool<T: DeserializeOwned>(
        &mut self,
        name: &str,
        arguments: &impl Serialize,
        deadline: Instant,
    ) -> Result<T, CallError> {
        let params = serde_json::json!({ "name": name, "arguments": arguments });
        let result: ToolResult = self
            .request("tools/call", &params, deadline)
            .and_then(read)?;
        let text = result
            .content
            .into_iter()
            .find_map(|content| (content.kind == "text").then_some(content.text))
            .unwrap_or_default();
        if result.is_error {
            return Err(CallError::Refused(text));
        }
        serde_json::from_str(&text)
            .map_err(|error| CallError::Unreadable(format!("hippius-mem's {name}: {error}")))
    }

    /// Sends request `method` and waits for its reply until `deadline`.
    fn request(
        &mut self,
        method: &str,
        params: &impl Serialize,
        deadline: Instant,
    ) -> Result<Value, CallError> {
        let id = self.next_id;
        self.next_id += 1;
        let line =
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.send(&line);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.replies.recv_timeout(left) {
                Ok(reply) if reply.id == id => {
                    return match reply.error {
                        Some(error) => Err(CallError::Refused(error.message)),
                        None => Ok(reply.result.unwrap_or(Value::Null)),
                    };
                }
                // A reply to an earlier request, which ran out of time, or a second reply.
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => return Err(CallError::TimedOut),
                Err(RecvTimeoutError::Disconnected) => return Err(self.exited()),
            }
        }
    }

    fn notify(&mut self, method: &str) {
        self.send(&serde_json::json!({ "jsonrpc": "2.0", "method": method }));
    }

    fn send(&mut self, message: &Value) {
        let mut line = message.to_string();
        line.push('\n');
        // A server that exited closed its input, and the write fails (Rust ignores SIGPIPE).
        // The error is dropped: the wait for the reply then sees the output end, after the
        // reader waited for the last words on standard error, which say why it exited (no
        // configuration, say). Failing here would report it without them.
        let written = self
            .stdin
            .write_all(line.as_bytes())
            .and_then(|()| self.stdin.flush());
        drop(written);
    }

    fn exited(&self) -> CallError {
        let tail = self.stderr.lock().unwrap_or_else(PoisonError::into_inner);
        CallError::Exited {
            stderr: String::from_utf8_lossy(&tail).trim().to_owned(),
        }
    }
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("pid", &self.child.id())
            .field("next_id", &self.next_id)
            .finish_non_exhaustive()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // The group first, and the process reaped after it, so its pid, the group's id,
        // cannot have been reused (agents::kill_group).
        agents::kill_group(self.child.id());
        // Fails when the group's kill already took it, or it had exited: both fine.
        drop(self.child.kill());
        drop(self.child.wait());
    }
}

/// One JSON-RPC reply, as the reader thread hands it over.
#[derive(Debug)]
struct Reply {
    id: u64,
    result: Option<Value>,
    error: Option<RpcError>,
}

/// Any message the server sends. Only replies (an id and no method) are read; its
/// notifications (log lines) and requests are dropped, as Postil declares no capability a
/// server request could need.
#[derive(Debug, Deserialize)]
struct Incoming {
    id: Option<Value>,
    method: Option<String>,
    result: Option<Value>,
    error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
struct RpcError {
    #[serde(default)]
    message: String,
}

/// Reads the server's output line by line, handing each reply to `replies`, until it ends or
/// the receiver is gone.
fn read_replies(stdout: impl Read, replies: &SyncSender<Reply>) {
    let mut stdout = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        // `read_until` and `skip_until` retry an interrupted read: an error is a real one.
        match (&mut stdout).take(LINE_LIMIT).read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if line.last() != Some(&b'\n') && line.len() as u64 == LINE_LIMIT {
            if stdout.skip_until(b'\n').is_err() {
                return;
            }
            continue;
        }
        let Ok(message) = serde_json::from_slice::<Incoming>(&line) else {
            continue;
        };
        let id = message.id.as_ref().and_then(Value::as_u64);
        let (Some(id), None) = (id, message.method) else {
            continue;
        };
        let reply = Reply {
            id,
            result: message.result,
            error: message.error,
        };
        if replies.send(reply).is_err() {
            return;
        }
    }
}

/// Keeps the last [`STDERR_TAIL`] bytes of `stderr` in `tail`. Reading also keeps the server
/// from blocking on a full pipe.
fn keep_tail(mut stderr: impl Read, tail: &Mutex<Vec<u8>>) {
    let mut buffer = [0; 4096];
    loop {
        let read = match stderr.read(&mut buffer) {
            Ok(0) => return,
            Ok(read) => read,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        let mut tail = tail.lock().unwrap_or_else(PoisonError::into_inner);
        tail.extend_from_slice(&buffer[..read]);
        let excess = tail.len().saturating_sub(STDERR_TAIL);
        tail.drain(..excess);
    }
}

fn spawn_thread(name: &str, run: impl FnOnce() + Send + 'static) -> io::Result<()> {
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(run)
        .map(drop)
}

fn read<T: DeserializeOwned>(value: Value) -> Result<T, CallError> {
    serde_json::from_value(value).map_err(|error| CallError::Unreadable(error.to_string()))
}

// ---- The messages ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Initialized {
    server_info: ServerInfo,
}

#[derive(Debug, Deserialize)]
struct ServerInfo {
    name: String,
    #[serde(default)]
    version: String,
}

#[derive(Debug, Deserialize)]
struct ToolList {
    tools: Vec<Tool>,
}

#[derive(Debug, Deserialize)]
struct Tool {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolResult {
    #[serde(default)]
    content: Vec<Content>,
    #[serde(default)]
    is_error: bool,
}

#[derive(Debug, Deserialize)]
struct Content {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

// ---- The tools' results, as the page gets them --------------------------------------------

/// What `recall` found: hippius-mem's pointers, best first.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct Recall {
    pointers: Vec<Pointer>,
}

/// One memory `recall` found. `updated` is in milliseconds since the Unix epoch.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct Pointer {
    id: String,
    summary: String,
    /// A low-confidence guess (hippius-mem 0.7.1 leaves the field out otherwise).
    #[serde(default)]
    weak: bool,
    author: String,
    updated: u64,
}

/// One memory whole: `get` and `history` together. Times are in milliseconds since the Unix
/// epoch.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Detail {
    id: String,
    note_type: String,
    author: String,
    created: u64,
    updated: u64,
    tags: Vec<String>,
    summary: String,
    body: String,
    history: Vec<Op>,
}

impl Detail {
    pub(super) fn new(note: Note, history: History) -> Self {
        Self {
            id: note.id,
            note_type: note.kind,
            author: note.author,
            created: note.created,
            updated: note.updated,
            tags: note.tags,
            summary: note.summary,
            body: note.body,
            history: history.entries.into_iter().map(Op::from).collect(),
        }
    }
}

/// One change to a memory, from its history.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Op {
    /// `Remember`, `Edit`, `Forget`, `Link`, ..., as hippius-mem names it.
    kind: String,
    author: String,
    anchor: Anchor,
}

impl From<HistoryEntry> for Op {
    fn from(entry: HistoryEntry) -> Self {
        let anchor = match entry.anchor {
            None => Anchor::None,
            Some(AnchorProof {
                reference: AnchorRef::Local(_),
            }) => Anchor::Local,
            Some(AnchorProof {
                reference: AnchorRef::OnChain(_),
            }) => Anchor::Chain,
        };
        Self {
            kind: entry.kind,
            author: entry.author,
            anchor,
        }
    }
}

/// Where an op's Merkle root was anchored, as `history` says. Postil does not check the
/// proof itself: it shows where the anchor is, and a local one's proof comes from the same
/// vault as the op.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Anchor {
    /// Not anchored yet.
    None,
    /// Anchored in the vault.
    Local,
    /// Committed on chain.
    Chain,
}

/// What `refresh` did: how many notes it indexed.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct Refreshed {
    indexed: u64,
}

/// `get`'s result.
#[derive(Debug, Deserialize)]
pub(super) struct Note {
    id: String,
    #[serde(rename = "note_type")]
    kind: String,
    author: String,
    created: u64,
    updated: u64,
    #[serde(default)]
    tags: Vec<String>,
    summary: String,
    body: String,
}

/// `history`'s result.
#[derive(Debug, Deserialize)]
pub(super) struct History {
    entries: Vec<HistoryEntry>,
}

#[derive(Debug, Deserialize)]
struct HistoryEntry {
    kind: String,
    author: String,
    anchor: Option<AnchorProof>,
}

#[derive(Debug, Deserialize)]
struct AnchorProof {
    reference: AnchorRef,
}

/// Where a root was anchored: hippius-mem's `AnchorRef`, an externally tagged enum whose
/// fields Postil does not read.
#[derive(Debug, Deserialize)]
enum AnchorRef {
    Local(IgnoredAny),
    OnChain(IgnoredAny),
}
