//! AG-UI event types.
//!
//! These map to the 16 standard event variants documented at
//! <https://docs.ag-ui.com/concepts/events>, plus two open-extensibility
//! variants (`Custom`, `Raw`) that AG-UI defines for arbitrary payloads.
//!
//! Wire format is JSON with `camelCase` keys, discriminated by a top-level
//! `type` field whose value is the PascalCase variant name (e.g.
//! `{"type":"RunStarted",...}`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Common metadata fields that may appear on any AG-UI event.
///
/// AG-UI servers may attach a `timestamp` (unix ms) and a `rawEvent`
/// echo of the upstream provider's raw payload. We carry these via
/// `#[serde(flatten)]` on every variant so they serialise/deserialise
/// at the same level as the variant's own fields.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BaseEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_event: Option<Value>,
}

/// AG-UI event.
///
/// Discriminated by the JSON `type` field. PascalCase to match the
/// JSON examples in the AG-UI concepts/events documentation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    RunStarted(RunStarted),
    RunFinished(RunFinished),
    RunError(RunError),
    StepStarted(StepStarted),
    StepFinished(StepFinished),
    TextMessageStart(TextMessageStart),
    TextMessageContent(TextMessageContent),
    TextMessageEnd(TextMessageEnd),
    TextMessageChunk(TextMessageChunk),
    ToolCallStart(ToolCallStart),
    ToolCallArgs(ToolCallArgs),
    ToolCallEnd(ToolCallEnd),
    ToolCallResult(ToolCallResult),
    StateSnapshot(StateSnapshot),
    StateDelta(StateDelta),
    MessagesSnapshot(MessagesSnapshot),
    Custom(Custom),
    Raw(Raw),
}

// ---------- Lifecycle ----------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunStarted {
    pub thread_id: String,
    pub run_id: String,
    #[serde(flatten)]
    pub base: BaseEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunFinished {
    pub thread_id: String,
    pub run_id: String,
    /// Optional terminal payload — e.g. `{ "type": "interrupt", ... }`
    /// when the run is paused waiting for user resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(flatten)]
    pub base: BaseEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunError {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(flatten)]
    pub base: BaseEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepStarted {
    pub step_name: String,
    #[serde(flatten)]
    pub base: BaseEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepFinished {
    pub step_name: String,
    #[serde(flatten)]
    pub base: BaseEvent,
}

// ---------- Text messages ----------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextMessageStart {
    pub message_id: String,
    /// AG-UI uses `"assistant"` for streamed assistant text. Optional
    /// because some servers omit it on continuations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(flatten)]
    pub base: BaseEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextMessageContent {
    pub message_id: String,
    pub delta: String,
    #[serde(flatten)]
    pub base: BaseEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextMessageEnd {
    pub message_id: String,
    #[serde(flatten)]
    pub base: BaseEvent,
}

/// Convenience event for servers that don't want to bracket every
/// chunk with `Start` / `End`. The chunk is treated as
/// `Start (if first) -> Content -> End (if final)` by the consumer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextMessageChunk {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub delta: String,
    #[serde(flatten)]
    pub base: BaseEvent,
}

// ---------- Tool calls ----------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallStart {
    pub tool_call_id: String,
    pub tool_call_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_message_id: Option<String>,
    #[serde(flatten)]
    pub base: BaseEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallArgs {
    pub tool_call_id: String,
    /// Argument JSON, streamed as a string delta (AG-UI convention —
    /// the server is mid-emitting an LLM tool call's argument JSON).
    pub delta: String,
    #[serde(flatten)]
    pub base: BaseEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallEnd {
    pub tool_call_id: String,
    #[serde(flatten)]
    pub base: BaseEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallResult {
    pub tool_call_id: String,
    pub message_id: String,
    /// Tool output. Free-form string per the AG-UI spec.
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(flatten)]
    pub base: BaseEvent,
}

// ---------- State ----------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateSnapshot {
    pub snapshot: Value,
    #[serde(flatten)]
    pub base: BaseEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateDelta {
    /// JSON Patch (RFC 6902) array. Kept as `Value` because callers
    /// often want to apply with `json-patch` or similar.
    pub delta: Value,
    #[serde(flatten)]
    pub base: BaseEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagesSnapshot {
    pub messages: Vec<Value>,
    #[serde(flatten)]
    pub base: BaseEvent,
}

// ---------- Open extensibility ----------

/// Server-defined event. AG-UI uses this for any payload that isn't
/// part of the 16 standard variants. The recursive `agui-tui` track
/// uses names prefixed with `agui-tui/` (e.g. `agui-tui/permission_request`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Custom {
    pub name: String,
    pub value: Value,
    #[serde(flatten)]
    pub base: BaseEvent,
}

/// Pass-through of an upstream provider's raw event (e.g. an OpenAI
/// chunk a server wants to forward verbatim).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Raw {
    pub event: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(flatten)]
    pub base: BaseEvent,
}
