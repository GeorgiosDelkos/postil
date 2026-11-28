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

/// The JSON-RPC error code an agent answers with when the engineer must sign in first: the
/// schema's `ErrorCode` "Authentication required", `auth_required`.
pub(crate) const AUTH_REQUIRED: i64 = -32000;

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
    pub(crate) params: Option<Box<serde_json::value::RawValue>>,
    #[serde(default)]
    pub(crate) result: Option<Box<serde_json::value::RawValue>>,
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

/// What the app can do for the agent: show a question as a form, and nothing else. It has no
/// unsaved buffers, so the agent's own file tools see the bytes the app shows, and it has no
/// terminal (docs/design.md, "ACP integration").
#[derive(Debug, Serialize)]
pub(crate) struct ClientCapabilities {
    pub(crate) fs: FsCapabilities,
    pub(crate) terminal: bool,
    pub(crate) elicitation: ElicitationCapabilities,
}

/// Form questions only (`{"form": {}}`): a URL question would send the engineer to a page
/// outside the app. Claude's adapter asks its questions (`AskUserQuestion`) this way only when
/// the client says it can; otherwise as permission requests, which Postil answers itself.
#[derive(Debug, Serialize)]
pub(crate) struct ElicitationCapabilities {
    pub(crate) form: Empty,
}

/// An empty JSON object, `{}`: how ACP says a capability is supported.
#[derive(Debug, Serialize)]
pub(crate) struct Empty {}

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
    /// The session's working directory, the run's worktree: an absolute path, as the protocol
    /// requires.
    pub(crate) cwd: &'a str,
    /// The MCP servers the agent starts for this session, besides those of its own
    /// configuration: hippius-mem when team memory is on, else none.
    pub(crate) mcp_servers: &'a [McpServer],
}

/// An MCP server the agent starts over stdio: the schema's `McpServerStdio`, the one form
/// every agent must support. It has no `type` field, which only the HTTP and SSE forms carry,
/// and no working directory: the agent starts it in the session's `cwd`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct McpServer {
    pub(crate) name: String,
    /// The program, by its absolute path, as the schema requires.
    pub(crate) command: String,
    pub(crate) args: Vec<String>,
    /// Set on top of the environment the agent gives it.
    pub(crate) env: Vec<EnvVariable>,
}

/// One variable of an [`McpServer`]'s environment.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct EnvVariable {
    pub(crate) name: String,
    pub(crate) value: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NewSessionResult {
    pub(crate) session_id: String,
    #[serde(default)]
    pub(crate) models: Option<SessionModels>,
    /// Extra metadata an agent attaches to `session/new` (Grok places its model info here).
    #[serde(default, rename = "_meta")]
    pub(crate) meta: Option<Value>,
    /// The session's settings the client may change, of any shape: each entry is read on its
    /// own by [`model_config`], so one of a shape the app does not know cannot fail the
    /// session.
    #[serde(default, deserialize_with = "null_as_default")]
    pub(crate) config_options: Vec<Value>,
}

/// The active model an agent reports in `session/new`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionModels {
    #[serde(default)]
    pub(crate) current_model_id: Option<String>,
}

/// The session setting that chooses the model: a select among `configOptions`. Every agent
/// probed on 2026-11-26 (agy, Grok, OpenCode, Claude's adapter) reports one, with id and
/// category `model`.
#[derive(Debug)]
pub(crate) struct ModelConfig {
    /// The option's id, which `session/set_config_option` names.
    pub(crate) id: String,
    /// The model in effect, when the agent says.
    pub(crate) current: Option<String>,
    /// The models offered, in the agent's order, groups flattened.
    pub(crate) options: Vec<ModelOption>,
}

impl ModelConfig {
    /// Whether `model` is one of the values offered.
    pub(crate) fn offers(&self, model: &str) -> bool {
        self.options.iter().any(|option| option.value == model)
    }
}

/// One model an agent offers: the id to send, and how to show it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub(crate) struct ModelOption {
    pub(crate) value: String,
    pub(crate) name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
}

/// The model select among `options`, an agent's `configOptions`: the first select whose
/// category is `model`, else the first select whose id is `model`. `None` when there is none:
/// the agent does not let the client choose. Options that cannot be read are skipped, and a
/// group of options (`{"group", "name", "options": [...]}`, which the schema allows in place
/// of a flat list) is read as its options, in order.
pub(crate) fn model_config(options: &[Value]) -> Option<ModelConfig> {
    let has = |option: &Value, field: &str, value: &str| {
        option.get(field).and_then(Value::as_str) == Some(value)
    };
    let selects = || {
        options
            .iter()
            .filter(|option| has(option, "type", "select"))
    };
    let select = selects()
        .find(|option| has(option, "category", "model"))
        .or_else(|| selects().find(|option| has(option, "id", "model")))?;
    let id = select.get("id").and_then(Value::as_str)?.to_owned();
    let current = select
        .get("currentValue")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let entries = select.get("options").and_then(Value::as_array);
    let read = |entry: &Value| serde_json::from_value::<ModelOption>(entry.clone()).ok();
    let options = entries
        .into_iter()
        .flatten()
        .flat_map(
            |entry| match entry.get("options").and_then(Value::as_array) {
                Some(group) => group.iter().filter_map(read).collect(),
                None => read(entry).into_iter().collect::<Vec<_>>(),
            },
        )
        .collect();
    Some(ModelConfig {
        id,
        current,
        options,
    })
}

/// `session/set_config_option` params.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SetConfigOptionParams<'a> {
    pub(crate) session_id: &'a str,
    pub(crate) config_id: &'a str,
    pub(crate) value: &'a str,
}

/// The `session/set_config_option` reply: the session's settings as they now are.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SetConfigOptionResult {
    #[serde(default, deserialize_with = "null_as_default")]
    pub(crate) config_options: Vec<Value>,
}

impl NewSessionResult {
    /// The model select the agent offers, if any ([`model_config`]).
    pub(crate) fn model_config(&self) -> Option<ModelConfig> {
        model_config(&self.config_options)
    }

    /// The active model id, when the agent reported one: the model select's current value,
    /// else `models.currentModelId`, else `_meta`.
    pub(crate) fn current_model(&self) -> Option<String> {
        let from_config = self.model_config().and_then(|config| config.current);
        let from_models = self
            .models
            .as_ref()
            .and_then(|models| models.current_model_id.as_deref());
        let from_meta = || {
            let meta = self.meta.as_ref()?;
            meta.get("currentModelId")
                .or_else(|| meta.get("model"))
                .or_else(|| {
                    meta.get("models")
                        .and_then(|models| models.get("currentModelId"))
                })
                .and_then(Value::as_str)
        };
        from_config
            .as_deref()
            .or(from_models)
            .or_else(from_meta)
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(str::to_owned)
    }
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
    /// Token counts the agent reports when a prompt turn ends (optional in ACP).
    #[serde(default)]
    usage: Option<PromptUsage>,
    /// Extra metadata an agent attaches to `session/prompt` (Grok places its per-turn token
    /// counts and `costUsdTicks` under `_meta.usage`).
    #[serde(default, rename = "_meta")]
    meta: Option<Value>,
}

/// Number of `costUsdTicks` per US dollar in xAI's usage responses (`10^10`).
const USD_TICKS_PER_DOLLAR: f64 = 10_000_000_000.0;

impl PromptResult {
    /// Token counts for the finished turn: ACP's top-level `usage` when non-empty, else
    /// `_meta` (`_meta.usage` then `_meta`, mapping Grok's `reasoningTokens` and
    /// `cacheCreationTokens` to ACP's fields).
    pub(crate) fn token_usage(&self) -> Option<PromptUsage> {
        if let Some(usage) = self.usage.filter(|u| *u != PromptUsage::default()) {
            return Some(usage);
        }
        let meta = self.meta.as_ref().filter(|v| v.is_object())?;
        let nested = meta.get("usage").filter(|v| v.is_object());
        let count = |keys: &[&str]| {
            [nested, Some(meta)].into_iter().flatten().find_map(|obj| {
                keys.iter()
                    .find_map(|key| obj.get(*key).and_then(u64_lenient))
            })
        };
        let usage = PromptUsage {
            input_tokens: count(&["inputTokens", "input_tokens"]),
            output_tokens: count(&["outputTokens", "output_tokens"]),
            thought_tokens: count(&["thoughtTokens", "reasoningTokens", "reasoning_tokens"]),
            cached_read_tokens: count(&["cachedReadTokens", "cache_read_input_tokens"]),
            cached_write_tokens: count(&[
                "cachedWriteTokens",
                "cacheCreationTokens",
                "cache_creation_input_tokens",
            ]),
            total_tokens: count(&["totalTokens", "total_tokens"]),
        };
        (usage != PromptUsage::default()).then_some(usage)
    }

    /// What this turn alone cost, when reported on the prompt reply (`_meta.usage.costUsdTicks`
    /// in Grok's ACP server). Unlike `usage_update`, this figure is per turn, not cumulative.
    pub(crate) fn turn_cost(&self) -> Option<UsageCost> {
        let meta = self.meta.as_ref().filter(|v| v.is_object())?;
        let nested = meta.get("usage").filter(|v| v.is_object());
        let ticks = [nested, Some(meta)].into_iter().flatten().find_map(|obj| {
            ["costUsdTicks", "cost_in_usd_ticks"]
                .into_iter()
                .find_map(|key| obj.get(key).and_then(u64_lenient))
        })?;
        #[expect(
            clippy::cast_precision_loss,
            reason = "u64 tick counts fit in f64 to sub-cent precision"
        )]
        let amount = (ticks as f64) / USD_TICKS_PER_DOLLAR;
        Some(UsageCost {
            amount,
            currency: "USD".to_owned(),
        })
    }
}

/// Reads an unsigned integer from a JSON number or a stringified integer (proto3 `int64`).
fn u64_lenient(val: &Value) -> Option<u64> {
    val.as_u64()
        .or_else(|| val.as_str()?.trim().parse::<u64>().ok())
}

/// Token counts reported on a `session/prompt` reply. Also the token part of a usage ledger
/// record (`usage/ledger.rs`), which stores them as received.
#[expect(
    clippy::struct_field_names,
    reason = "the fields are named as the protocol names them"
)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PromptUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) thought_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cached_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cached_write_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) total_tokens: Option<u64>,
}

/// Cost reported by an agent: cumulative on `usage_update`, or per turn on `session/prompt`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(PartialEq))]
#[serde(rename_all = "camelCase")]
pub(crate) struct UsageCost {
    pub(crate) amount: f64,
    pub(crate) currency: String,
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
#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(test, derive(PartialEq, Eq))]
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
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(PartialEq))]
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
    /// Context window and optional cumulative cost reported while a session runs.
    #[serde(rename_all = "camelCase")]
    UsageUpdate {
        #[serde(default)]
        used: Option<u64>,
        #[serde(default)]
        size: Option<u64>,
        #[serde(default)]
        cost: Option<UsageCost>,
    },
    /// The echo of the prompt, the command list, mode changes, and kinds added
    /// after the pinned schema: none of them changes what the run panel shows.
    #[serde(other)]
    Other,
}

/// A tool call the agent started.
#[expect(
    clippy::struct_field_names,
    reason = "the fields are named as the protocol names them"
)]
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(PartialEq, Eq))]
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
    /// The tool's input, in the agent's own shape: the run log reads an execute call's
    /// command from it (`run/log.rs`, `command_of`).
    pub(crate) raw_input: Option<Value>,
    /// The tool's output, in the agent's own shape: the run log reads an execute call's exit
    /// code from it (`run/log.rs`, `exit_of`).
    pub(crate) raw_output: Option<Value>,
}

/// A change to a tool call: only the fields that changed are present.
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(PartialEq, Eq))]
#[serde(rename_all = "camelCase")]
pub(crate) struct ToolCallUpdate {
    pub(crate) tool_call_id: String,
    pub(crate) title: Option<String>,
    pub(crate) kind: Option<ToolKind>,
    pub(crate) status: Option<ToolCallStatus>,
    pub(crate) content: Option<Vec<ToolCallContent>>,
    pub(crate) locations: Option<Vec<Location>>,
    pub(crate) raw_input: Option<Value>,
    pub(crate) raw_output: Option<Value>,
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
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(PartialEq, Eq))]
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
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub(crate) struct Location {
    pub(crate) path: String,
    pub(crate) line: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(PartialEq, Eq))]
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
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(PartialEq, Eq))]
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

/// A question for the engineer: `elicitation/create` in ACP's unstable schema at the pinned
/// commit, `session/create_elicitation` in Claude's adapter, which follows a later SDK.
/// `requested_schema` is a JSON Schema object whose properties are the form's fields; the
/// page draws it. A URL question (`mode` "url") has none.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ElicitationParams {
    #[serde(default)]
    pub(crate) mode: Option<String>,
    pub(crate) message: String,
    #[serde(default)]
    pub(crate) requested_schema: Option<Value>,
    #[serde(default)]
    pub(crate) tool_call_id: Option<String>,
}

/// The app's answer to a question: the form's values, a skip, or the turn's cancel.
#[derive(Debug, Serialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub(crate) enum ElicitationReply {
    Accept {
        content: serde_json::Map<String, Value>,
    },
    Decline,
    Cancel,
}

/// `$/cancel_request` params: the agent withdraws a request it sent, such as a question no
/// longer needed.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CancelRequestParams {
    pub(crate) request_id: Value,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_result_prefers_standard_usage_and_falls_back_to_grok_meta_usage() {
        let standard: PromptResult = serde_json::from_value(serde_json::json!({
            "stopReason": "end_turn",
            "usage": {
                "inputTokens": 100,
                "outputTokens": 20,
                "thoughtTokens": 5,
                "totalTokens": 120
            },
            "_meta": {
                "usage": { "inputTokens": 999, "costUsdTicks": 50_000_000 }
            }
        }))
        .unwrap();
        assert_eq!(
            standard.token_usage(),
            Some(PromptUsage {
                input_tokens: Some(100),
                output_tokens: Some(20),
                thought_tokens: Some(5),
                total_tokens: Some(120),
                ..PromptUsage::default()
            })
        );
        let cost = standard.turn_cost().unwrap();
        assert!((cost.amount - 0.005).abs() < 1e-9);
        assert_eq!(cost.currency, "USD");

        let grok: PromptResult = serde_json::from_value(serde_json::json!({
            "stopReason": "end_turn",
            "_meta": {
                "totalTokens": 22259,
                "usage": {
                    "inputTokens": 22137,
                    "outputTokens": 114,
                    "totalTokens": 22251,
                    "cachedReadTokens": 1664,
                    "cacheCreationTokens": 12,
                    "reasoningTokens": 113,
                    "costUsdTicks": 144_370_800
                }
            }
        }))
        .unwrap();
        assert_eq!(
            grok.token_usage(),
            Some(PromptUsage {
                input_tokens: Some(22137),
                output_tokens: Some(114),
                thought_tokens: Some(113),
                cached_read_tokens: Some(1664),
                cached_write_tokens: Some(12),
                total_tokens: Some(22251),
            })
        );
        let grok_cost = grok.turn_cost().unwrap();
        assert!((grok_cost.amount - 0.014_437_08).abs() < 1e-12);

        let empty: PromptResult = serde_json::from_value(serde_json::json!({
            "stopReason": "end_turn",
            "_meta": { "sessionId": "s1" }
        }))
        .unwrap();
        assert_eq!(empty.token_usage(), None);
        assert_eq!(empty.turn_cost(), None);

        // An empty top-level `usage: {}` does not shadow `_meta`, stringified proto3 `int64`
        // ticks parse, and `null` primary keys fall back to snake_case / outer `_meta`.
        let fallback: PromptResult = serde_json::from_value(serde_json::json!({
            "stopReason": "end_turn",
            "usage": {},
            "_meta": {
                "costUsdTicks": null,
                "cost_in_usd_ticks": " 250000000 ",
                "total_tokens": "15",
                "usage": {
                    "inputTokens": null,
                    "input_tokens": "10",
                    "output_tokens": 5,
                    "reasoning_tokens": 2,
                    "cache_read_input_tokens": 3,
                    "cache_creation_input_tokens": 1
                }
            }
        }))
        .unwrap();
        assert_eq!(
            fallback.token_usage(),
            Some(PromptUsage {
                input_tokens: Some(10),
                output_tokens: Some(5),
                thought_tokens: Some(2),
                cached_read_tokens: Some(3),
                cached_write_tokens: Some(1),
                total_tokens: Some(15),
            })
        );
        let fallback_cost = fallback.turn_cost().unwrap();
        assert!((fallback_cost.amount - 0.025).abs() < 1e-12);
    }
}
