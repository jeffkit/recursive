//! `RunAgentInput` and supporting types — the request payload an
//! AG-UI client sends to start (or resume) a run.
//!
//! The AG-UI spec is fuzzy on the exact shape of `messages`, `tools`,
//! `context`, and `resume`. We model the named types as structs with
//! the well-known fields and fall back to `serde_json::Value` for
//! everything else, so we round-trip unknown fields without losing data.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Request body for `POST <agui-endpoint>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunAgentInput {
    pub thread_id: String,
    pub run_id: String,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Vec<Tool>,
    #[serde(default)]
    pub context: Vec<ContextItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<Vec<Resume>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<Value>,
    /// Test-only trigger: interrupt the run before executing any tool
    /// whose name appears in this list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupt_before: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwarded_props: Option<Value>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: String,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextItem {
    pub description: String,
    pub value: String,
}

/// One resumed interaction — an answer to an interrupt the server
/// raised on a prior run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resume {
    pub interrupt_id: String,
    pub status: ResumeStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
}

/// Status of a resolved interrupt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResumeStatus {
    Resolved,
    Cancelled,
}
