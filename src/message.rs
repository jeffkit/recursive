//! Chat message primitive.
//!
//! A `Message` mirrors the wire format that most chat completion APIs use,
//! but stays provider-agnostic. Providers translate to/from their own shape
//! in their adapter.

use serde::{Deserialize, Serialize};

use crate::llm::ToolCall;

/// Serde helper: skip serializing `is_compaction_summary` when false, so old
/// JSONL transcripts on disk deserialize cleanly without the field.
#[inline]
fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// DeepSeek reasoning/thinking content. Must be echoed back to the API
    /// when present, otherwise the API returns a 400 error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// True when this message was inserted by `Compactor` as a summary of
    /// older messages. Used by AgentKernel::run to detect intra-turn
    /// compaction summaries without sniffing the rendered content text.
    /// Defaults to false; old transcripts on disk serialize as false
    /// (field omitted).
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_compaction_summary: bool,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            reasoning_content: None,
            is_compaction_summary: false,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            reasoning_content: None,
            is_compaction_summary: false,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            reasoning_content: None,
            is_compaction_summary: false,
        }
    }

    pub fn assistant_with_tool_calls(
        content: impl Into<String>,
        tool_calls: Vec<ToolCall>,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls,
            tool_call_id: None,
            reasoning_content: None,
            is_compaction_summary: false,
        }
    }

    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
            reasoning_content: None,
            is_compaction_summary: false,
        }
    }

    /// Mark this message as a compaction summary.
    ///
    /// Used by `Compactor` so that `AgentKernel::run` can detect intra-turn
    /// compaction summaries via a typed field instead of sniffing the
    /// rendered content text.
    pub fn with_compaction_summary(mut self) -> Self {
        self.is_compaction_summary = true;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_summary_bit_default_false() {
        let m = Message::user("hi");
        assert!(!m.is_compaction_summary);
    }

    #[test]
    fn with_compaction_summary_sets_bit() {
        let m = Message::system("...").with_compaction_summary();
        assert!(m.is_compaction_summary);
    }

    #[test]
    fn compaction_summary_bit_omitted_from_json_when_false() {
        let m = Message::user("hi");
        let json = serde_json::to_string(&m).unwrap();
        assert!(!json.contains("is_compaction_summary"));
    }

    #[test]
    fn compaction_summary_bit_included_in_json_when_true() {
        // Kills: `replace is_false -> bool with true` (always skip).
        // When `is_compaction_summary = true`, the field MUST appear in JSON
        // so that `AgentKernel::run` can detect intra-turn summaries after
        // deserialising a transcript from disk.
        let m = Message::system("summary").with_compaction_summary();
        let json = serde_json::to_string(&m).unwrap();
        assert!(
            json.contains("\"is_compaction_summary\":true"),
            "is_compaction_summary must be serialised when true; got: {json}"
        );

        // Round-trip: deserialise back and verify the flag is preserved.
        let restored: Message = serde_json::from_str(&json).unwrap();
        assert!(
            restored.is_compaction_summary,
            "is_compaction_summary must survive a serde round-trip"
        );
    }
}
