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
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BaseEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_event: Option<Value>,
}

/// AG-UI event, discriminated by JSON `type` field.
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

/// Discriminated outcome for a finished run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum RunFinishedOutcome {
    Success,
    Interrupt { interrupts: Vec<Interrupt> },
}

/// One open interrupt that the client must resolve.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Interrupt {
    pub id: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunFinished {
    pub thread_id: String,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<RunFinishedOutcome>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Custom {
    pub name: String,
    pub value: Value,
    #[serde(flatten)]
    pub base: BaseEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Raw {
    pub event: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(flatten)]
    pub base: BaseEvent,
}
