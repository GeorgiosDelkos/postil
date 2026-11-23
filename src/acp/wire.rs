//! The ACP messages Postil reads and writes, as serde types.
//!
//! Written from the version 1 schema at a pinned commit:
//! <https://github.com/agentclientprotocol/agent-client-protocol/blob/01a54c84f39508f73fab4d41362baf51e1320374/schema/v1/schema.json>
//!
//! Only the fields the app uses are declared, and unknown fields are ignored, because agents
//! add their own (Grok puts its model list in `_meta`). The schema marks most of what the app
//! reads as "default on error", so the app reads leniently where a message would otherwise be
//! lost: an enum value it does not know reads as `Other` (a newer agent's tool kind must not
//! hide its tool call), a `null` list or kind reads as its default, and an error object reads
//! whatever code and message it has. A value of another type (a number for a title) still
//! makes the message unreadable, and the run says so.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// The ACP version this client speaks. Version 2 has been a draft since 2026-07-20 and
/// changes how a turn ends, so the app does not negotiate it (docs/design.md, "ACP
/// integration").
pub(crate) const PROTOCOL_VERSION: u32 = 1;

/// A JSON-RPC 2.0 request from the app. Every request the app sends has a numeric id.
#[derive(Debug, Serialize)]
pub(crate) struct Request<'a, P> {
    pub(crate) jsonrpc: &'static str,
    pub(crate) id: u64,
    pub(crate) method: &'a str,
    pub(crate) params: P,
}

/// A JSON-RPC 2.0 notification from the app: no id, no reply.
#[derive(Debug, Serialize)]
pub(crate) struct Notification<'a, P> {
    pub(crate) jsonrpc: &'static str,
    pub(crate) method: &'a str,
    pub(crate) params: P,
}

/// The app's reply to a request from the agent.
#[derive(Debug, Serialize)]
pub(crate) struct Response<'a, R> {
    pub(crate) jsonrpc: &'static str,
    /// The agent's id, echoed: a number or a string.
    pub(crate) id: &'a Value,
    #[serde(flatten)]
    pub(crate) outcome: R,
}

/// The `result` or `error` member of a reply.
#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Outcome<R> {
    Result(R),
    Error(RpcError),
}

/// Any message from the agent, before it is told apart: a reply to the app (an `id` and no
/// `method`), a request (both), or a notification (a `method` and no `id`).
#[derive(Debug, Deserialize)]
pub(crate) struct Incoming {
    pub(crate) id: Option<Value>,
    pub(crate) method: Option<String>,
    #[serde(default)]
    pub(crate) params: Value,
    pub(crate) result: Option<Value>,
    /// Read leniently by [`RpcError::from_value`]: a malformed error must still answer its
    /// request, or the caller would wait for a reply that has come.
    pub(crate) error: Option<Value>,
}

/// A JSON-RPC error object.
#[derive(Debug, Serialize)]
pub(crate) struct RpcError {
    pub(crate) code: i64,
    pub(crate) message: String,
}

impl RpcError {
    /// An error object as an agent sent it: its `code` (0 when missing) and its `message`
    /// (the whole object when missing).
    pub(crate) fn from_value(error: &Value) -> Self {
        Self {
            code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .map_or_else(|| error.to_string(), str::to_owned),
        }
    }
}

/// Reads `null` as the default, as a missing field already reads with `#[serde(default)]`.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// JSON-RPC's code for a method the receiver does not implement.
pub(crate) const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC's code for params the receiver cannot read.
pub(crate) const INVALID_PARAMS: i64 = -32602;

/// `initialize` params.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InitializeParams {
    pub(crate) protocol_version: u32,
    pub(crate) client_capabilities: ClientCapabilities,
}

/// What the app can do for the agent: nothing. It has no unsaved buffers, so the agent's own
/// file tools see the bytes the app shows, and it has no terminal (docs/design.md, "ACP
/// integration").
#[derive(Debug, Serialize)]
pub(crate) struct ClientCapabilities {
    pub(crate) fs: FsCapabilities,
    pub(crate) terminal: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FsCapabilities {
    pub(crate) read_text_file: bool,
    pub(crate) write_text_file: bool,
}

/// The part of the `initialize` reply the app checks.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InitializeResult {
    pub(crate) protocol_version: u32,
}

/// `session/new` params.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NewSessionParams<'a> {
    /// The repository root: an absolute path, as the protocol requires.
    pub(crate) cwd: &'a str,
    /// Always empty: the agent keeps the MCP servers of its own configuration.
    pub(crate) mcp_servers: [(); 0],
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NewSessionResult {
    pub(crate) session_id: String,
}

/// `session/prompt` params.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PromptParams<'a> {
    pub(crate) session_id: &'a str,
    pub(crate) prompt: &'a [ContentBlock],
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PromptResult {
    pub(crate) stop_reason: StopReason,
}

/// Why a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
    #[serde(other)]
    Other,
}

/// `session/cancel` params.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CancelParams<'a> {
    pub(crate) session_id: &'a str,
}

/// A block of a prompt or of a message. The app sends text and links; of what agents send,
/// it shows text, and names a link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ContentBlock {
    Text {
        text: String,
    },
    ResourceLink {
        uri: String,
        name: String,
    },
    /// An image, audio, or embedded resource. Never sent by the app.
    #[serde(other)]
    Other,
}

/// `session/update` params.
#[derive(Debug, Deserialize)]
pub(crate) struct UpdateParams {
    pub(crate) update: SessionUpdate,
}

/// One change to the session the agent reports while it works.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub(crate) enum SessionUpdate {
    #[serde(rename_all = "camelCase")]
    AgentMessageChunk {
        content: ContentBlock,
        /// Which message the part belongs to: the agent's id, when it sends one. Claude's
        /// adapter does (seen 2026-11-23); without it, parts in a row are one message.
        message_id: Option<String>,
    },
    AgentThoughtChunk {
        content: ContentBlock,
    },
    ToolCall(ToolCall),
    ToolCallUpdate(ToolCallUpdate),
    Plan {
        #[serde(default, deserialize_with = "null_as_default")]
        entries: Vec<PlanEntry>,
    },
    /// The echo of the prompt, the command list, mode and usage changes, and kinds added
    /// after the pinned schema: none of them changes what the run panel shows.
    #[serde(other)]
    Other,
}

/// A tool call the agent started.
#[expect(
    clippy::struct_field_names,
    reason = "the fields are named as the protocol names them"
)]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ToolCall {
    pub(crate) tool_call_id: String,
    pub(crate) title: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(crate) kind: ToolKind,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(crate) status: ToolCallStatus,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(crate) content: Vec<ToolCallContent>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(crate) locations: Vec<Location>,
}

/// A change to a tool call: only the fields that changed are present.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ToolCallUpdate {
    pub(crate) tool_call_id: String,
    pub(crate) title: Option<String>,
    pub(crate) kind: Option<ToolKind>,
    pub(crate) status: Option<ToolCallStatus>,
    pub(crate) content: Option<Vec<ToolCallContent>>,
    pub(crate) locations: Option<Vec<Location>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    SwitchMode,
    /// The schema's default, and any kind added after the pinned schema.
    #[default]
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToolCallStatus {
    /// The schema's default.
    #[default]
    Pending,
    InProgress,
    Completed,
    Failed,
    #[serde(other)]
    Other,
}

/// What a tool call produced.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ToolCallContent {
    Content {
        content: ContentBlock,
    },
    #[serde(rename_all = "camelCase")]
    Diff {
        path: String,
        /// `None` for a new file.
        old_text: Option<String>,
        new_text: String,
    },
    /// A terminal (the app offers none, so agents should not send it) or a kind added after
    /// the pinned schema.
    #[serde(other)]
    Other,
}

/// A file a tool call touches, and the line when the agent knows it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct Location {
    pub(crate) path: String,
    pub(crate) line: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct PlanEntry {
    pub(crate) content: String,
    pub(crate) status: PlanEntryStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlanEntryStatus {
    Pending,
    InProgress,
    Completed,
    #[serde(other)]
    Other,
}

/// `session/request_permission` params.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PermissionParams {
    pub(crate) tool_call: ToolCallUpdate,
    pub(crate) options: Vec<PermissionOption>,
}

/// One answer the agent offers to a permission request.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PermissionOption {
    pub(crate) option_id: String,
    pub(crate) name: String,
    pub(crate) kind: PermissionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PermissionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
    #[serde(other)]
    Other,
}

/// The app's answer to a permission request.
#[derive(Debug, Serialize)]
pub(crate) struct PermissionResult<'a> {
    pub(crate) outcome: PermissionOutcome<'a>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum PermissionOutcome<'a> {
    /// The turn was cancelled; the protocol requires this answer to every open request.
    Cancelled,
    #[serde(rename_all = "camelCase")]
    Selected { option_id: &'a str },
}
