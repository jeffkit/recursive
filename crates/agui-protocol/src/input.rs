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
    /// Forward-compatible escape hatch. Servers may attach extra
    /// per-run config; clients that don't care can pass `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwarded_props: Option<Value>,
}

/// A chat message. The AG-UI spec defers to the OpenAI message shape;
/// we keep the canonical fields named and stuff anything else into
/// `extra`. Tool-call attachments are kept as `Value` for the same
/// reason — we don't want to enforce a schema we'd then have to keep
/// in lockstep with multiple providers.
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

/// A tool the agent may call. Schema is `Value` because tool schemas
/// are themselves JSON Schema objects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// A piece of context the client wants the agent to consider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextItem {
    pub description: String,
    pub value: String,
}

/// One resumed interaction — typically an answer to an interrupt
/// (e.g. permission Y/N) the server raised on a prior run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resume {
    pub id: String,
    pub value: Value,
}
