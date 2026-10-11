//! Chat message primitive.
//!
//! A `Message` mirrors the wire format that most chat completion APIs use,
//! but stays provider-agnostic. Providers translate to/from their own shape
//! in their adapter.

use serde::{Deserialize, Serialize};

/// A structured request to invoke one of the registered tools.
///
/// Part of the assistant message shape (a field of `Message`), so it lives
/// here rather than in the provider layer; the llm module re-exports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON arguments as produced by the model.
    pub arguments: serde_json::Value,
}

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

/// Issue #168 — the "poisoned assistant message" predicate.
///
/// An assistant message with no text and no tool calls carries no
/// information, but it is *not* harmless on the wire: Anthropic (and every
/// strict OpenAI→Anthropic translation gateway) rejects an empty text block
/// with HTTP 400 "text content blocks must be non-empty". A message of this
/// shape therefore poisons the whole session — every later request that
/// replays the transcript fails.
///
/// The single definition is shared by all four guard sites so they can never
/// drift apart:
///
/// * `run_core::finish_interrupted` — never *push* the empty shape;
/// * `session::reader::SessionReader::load_messages` — filter rows already
///   written to disk by a pre-fix binary, so a poisoned session is still
///   openable;
/// * `llm::openai::build_request` / `llm::anthropic::serialize_messages_anthropic`
///   — belt-and-braces drop on the request path.
///
/// Deliberately narrow: `content` is trimmed (a whitespace-only reply is
/// equally empty on the wire) and messages carrying `tool_calls` are NEVER
/// matched, because dropping one would orphan its `Role::Tool` results and
/// break invariant #8.
#[cfg_attr(test, mutants::skip)]
pub fn is_empty_assistant(m: &Message) -> bool {
    m.role == Role::Assistant && m.content.trim().is_empty() && m.tool_calls.is_empty()
}

/// Issue #168 — is this interrupted assistant message worth persisting?
///
/// [`is_empty_assistant`] is the right predicate for the *serializers* and the
/// *load filter*: those drop the poisoned row and nothing else. It is
/// deliberately **not** the right predicate for `run_core::finish_interrupted`,
/// which decides whether to write a new row at all.
///
/// The issue's 「不做」 clause pins that: "不改 reasoning-only 中断的处理（那是
/// 另一种形态）". A reasoning-only reply (`content == ""`,
/// `reasoning_content == Some(_)`) is a real model output — DeepSeek thinking
/// mode and Anthropic extended thinking both emit thinking deltas before any
/// text — so discarding it would silently drop the turn's work. Keeping it is
/// also wire-safe: the Anthropic serializer maps a `reasoning_content`-bearing
/// message with empty content to a bare `{"role":"assistant","content":[]}`
/// (no empty text block), and the OpenAI serializer reports the thought via
/// `reasoning_content` with `content: null` (#16).
///
/// So: persist when there is text **or** reasoning; drop only when there is
/// neither. Same carve-out for `tool_calls`: such a message is never
/// "nothing".
#[cfg_attr(test, mutants::skip)]
pub fn has_persistable_interrupt(m: &Message) -> bool {
    m.role == Role::Assistant
        && (!m.content.trim().is_empty()
            || m.reasoning_content
                .as_deref()
                .is_some_and(|r| !r.trim().is_empty())
            || !m.tool_calls.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_call(id: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: "Read".to_string(),
            arguments: serde_json::json!({"path": "x"}),
        }
    }

    fn assistant_with_reasoning(reasoning: &str) -> Message {
        let mut m = Message::assistant("");
        m.reasoning_content = Some(reasoning.to_string());
        m
    }

    // ---- issue #168: is_empty_assistant ---------------------------------

    #[test]
    fn is_empty_assistant_matches_empty_and_whitespace_only() {
        assert!(is_empty_assistant(&Message::assistant("")));
        assert!(
            is_empty_assistant(&Message::assistant("  \n\t ".to_string())),
            "whitespace-only content is empty on the wire",
        );
    }

    #[test]
    fn is_empty_assistant_rejects_non_empty_content() {
        assert!(!is_empty_assistant(&Message::assistant("hi")));
        assert!(!is_empty_assistant(&Message::assistant(" x ")));
    }

    #[test]
    fn is_empty_assistant_never_matches_a_tool_call_carrier() {
        // Invariant #8: dropping a tool_call assistant would orphan its
        // `Role::Tool` results.
        let m = Message::assistant_with_tool_calls("", vec![tool_call("call_1")]);
        assert!(!is_empty_assistant(&m));
    }

    #[test]
    fn is_empty_assistant_rejects_every_other_role() {
        // Kills `role == Assistant -> !=` / `-> true` style replacements: only
        // the assistant role can be the poisoned shape.
        assert!(!is_empty_assistant(&Message::user("")));
        assert!(!is_empty_assistant(&Message::system("")));
        assert!(!is_empty_assistant(&Message::tool_result("call_1", "")));
    }

    #[test]
    fn is_empty_assistant_ignores_reasoning() {
        // The serializers/load filter drop the exact poisoned shape; a
        // reasoning-only row is *not* it (the interrupt carve-out lives in
        // `has_persistable_interrupt`).
        assert!(is_empty_assistant(&assistant_with_reasoning(
            "thinking hard"
        )));
    }

    // ---- issue #168: has_persistable_interrupt --------------------------

    #[test]
    fn has_persistable_interrupt_keeps_text_and_reasoning_only() {
        assert!(has_persistable_interrupt(&Message::assistant("partial")));
        assert!(
            has_persistable_interrupt(&assistant_with_reasoning("thinking hard")),
            "reasoning-only interrupts must still be persisted (issue #168 不做 clause)",
        );
        assert!(has_persistable_interrupt(
            &Message::assistant_with_tool_calls("", vec![tool_call("call_1")],)
        ));
    }

    #[test]
    fn has_persistable_interrupt_drops_only_nothing() {
        assert!(!has_persistable_interrupt(&Message::assistant("")));
        assert!(!has_persistable_interrupt(&Message::assistant("  ")));
        // Empty/whitespace reasoning is not reasoning.
        let mut blank = Message::assistant("");
        blank.reasoning_content = Some(String::new());
        assert!(!has_persistable_interrupt(&blank));
        let mut ws = Message::assistant("");
        ws.reasoning_content = Some("  ".to_string());
        assert!(!has_persistable_interrupt(&ws));
    }

    #[test]
    fn has_persistable_interrupt_rejects_non_assistant_roles() {
        assert!(!has_persistable_interrupt(&Message::user("hello")));
        assert!(!has_persistable_interrupt(&Message::system("prompt")));
        assert!(!has_persistable_interrupt(&Message::tool_result(
            "call_1", "body"
        )));
    }

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
        use super::ToolCall;
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
