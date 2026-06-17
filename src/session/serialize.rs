//! Transcript entry serialization types and conversion to runtime [`Message`]s.
//!
//! Split from `session.rs` during the Goal 221 module refactor.

use serde::{Deserialize, Serialize};

/// A single JSONL line representing one message in the transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptEntry {
    /// Stable UUID v4 for this message (g155). Empty string for pre-g155 entries.
    #[serde(default)]
    pub uuid: String,
    /// UUID of the parent message in the conversation chain (g155).
    /// `None` for the root message (first message in a session or chain branch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_uuid: Option<String>,
    /// For tool-result messages, the UUID of the assistant message that issued
    /// the tool call (g155). Enables correct attribution in multi-agent trees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_tool_assistant_uuid: Option<String>,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<crate::llm::ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Token usage for this message (non-None for assistant messages, g156).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<super::UsageMeta>,
    pub timestamp: String,
    /// Goal-153: per-call audit metadata. Only populated on `role: "tool"`
    /// messages (i.e. tool results). Never sent to providers — see
    /// [`entry_to_message`] which drops this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit: Option<crate::tools::AuditMeta>,
}

/// A compact-boundary system entry written to the JSONL when cross-turn
/// compaction fires (g157). Parsed by `SessionReader::load_transcript`
/// to skip pre-boundary messages on resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CompactBoundaryEntry {
    #[serde(rename = "type")]
    pub(crate) entry_type: String,
    pub(crate) subtype: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) turn: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) compacted_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) summary_uuid: Option<String>,
    pub(crate) timestamp: String,
}

/// Convert a persisted [`TranscriptEntry`] into a runtime [`Message`].
///
/// Drops persistence-only fields (`id`, `parent_id`, `timestamp`,
/// and — once g153 lands — `audit`). The result is what
/// `run_resumed` expects as its `seed` argument and what provider
/// adapters serialise onto the wire.
///
/// This is the **isolation point** between the persisted shape
/// (`TranscriptEntry`) and the LLM wire shape (`Message`):
/// provider adapters never see persistence-only fields. An unknown
/// `role` string maps to `Role::User` defensively so a corrupted
/// transcript can't panic the resume handler — in practice this
/// never fires because the writer only ever emits the four known
/// roles.
pub fn entry_to_message(entry: TranscriptEntry) -> crate::message::Message {
    use crate::message::Role;
    let role = match entry.role.as_str() {
        "system" => Role::System,
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        _ => Role::User,
    };
    crate::message::Message {
        role,
        content: entry.content,
        tool_calls: entry.tool_calls,
        tool_call_id: entry.tool_call_id,
        reasoning_content: entry.reasoning_content,
        is_compaction_summary: false,
    }
}
