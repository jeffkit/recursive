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

    #[test]
    fn is_false_returns_true_for_false_value() {
        // kills `replace !*b with *b` mutation in is_false()
        assert!(is_false(&false));
    }

    #[test]
    fn is_false_returns_false_for_true_value() {
        // complementary: is_false(true) must be false
        assert!(!is_false(&true));
    }

    #[test]
    fn constructor_roles_are_correct() {
        // kills any mutation that swaps Role variants in constructors
        assert_eq!(Message::system("").role, Role::System);
        assert_eq!(Message::user("").role, Role::User);
        assert_eq!(Message::assistant("").role, Role::Assistant);
        assert_eq!(
            Message::assistant_with_tool_calls("", Vec::new()).role,
            Role::Assistant
        );
        assert_eq!(Message::tool_result("id", "").role, Role::Tool);
    }

    #[test]
    fn tool_result_sets_tool_call_id() {
        // kills `tool_call_id: None` mutation in tool_result()
        let m = Message::tool_result("call-123", "output");
        assert_eq!(m.tool_call_id.as_deref(), Some("call-123"));
    }

    #[test]
    fn constructors_set_empty_tool_calls_by_default() {
        // kills mutations that set tool_calls: vec![<something>] in constructors
        assert!(Message::system("").tool_calls.is_empty());
        assert!(Message::user("").tool_calls.is_empty());
        assert!(Message::assistant("").tool_calls.is_empty());
        assert!(Message::tool_result("", "").tool_calls.is_empty());
    }

    #[test]
    fn assistant_with_tool_calls_stores_tool_calls() {
        // kills `tool_calls: vec![]` mutation in assistant_with_tool_calls
        use crate::llm::ToolCall;
        let tc = ToolCall {
            id: "tc-1".to_string(),
            name: "Read".to_string(),
            arguments: serde_json::json!({}),
        };
        let m = Message::assistant_with_tool_calls("calling", vec![tc]);
        assert_eq!(m.tool_calls.len(), 1, "tool_calls must be stored");
        assert_eq!(m.tool_calls[0].id, "tc-1");
    }

    #[test]
    fn constructors_store_content() {
        // kills `content: "".into()` or `content: Default::default()` mutations
        assert_eq!(Message::system("sys-content").content, "sys-content");
        assert_eq!(Message::user("user-content").content, "user-content");
        assert_eq!(Message::assistant("asst-content").content, "asst-content");
        assert_eq!(Message::tool_result("id", "tool-out").content, "tool-out");
    }
}
