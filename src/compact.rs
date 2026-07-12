//! LLM-driven context compaction.
//!
//! When the transcript grows large, `Compactor::compact` asks the model to
//! summarize the older portion into a single system message, preserving key
//! decisions, paths, and outcomes. The agent then continues with the summary
//! plus recent messages, staying within the context window.
//!
//! Compaction is **disabled by default** (threshold = `usize::MAX`). Enable
//! it via `AgentBuilder::compactor(...)`.

use crate::error::Result;
use crate::llm::{ChatProvider, StructuredRequest, ToolSpec};
use crate::message::Message;

/// Configuration for LLM-driven transcript compaction.
#[derive(Debug, Clone)]
pub struct Compactor {
    /// Character-count threshold above which compaction is triggered.
    /// Defaults to `usize::MAX` (disabled).
    ///
    /// This estimate is reliable for English content (~4 chars/token) but
    /// underestimates token density in CJK languages. When actual
    /// `prompt_tokens` from the API are available, `threshold_prompt_tokens`
    /// is checked first and takes priority.
    pub threshold_chars: usize,
    /// Token-count threshold: if the last turn's `prompt_tokens` from the
    /// API response meets or exceeds this value, compaction is triggered.
    ///
    /// When `Some` and the last prompt_tokens is non-zero, this check takes
    /// priority over `threshold_chars`. This eliminates the 4-char/token
    /// assumption that fails for CJK content. Set to `None` to rely solely
    /// on the char-based heuristic.
    pub threshold_prompt_tokens: Option<u32>,
    /// Number of most-recent messages to keep verbatim during compaction.
    pub keep_recent_n: usize,
}

impl Default for Compactor {
    fn default() -> Self {
        Self {
            threshold_chars: usize::MAX,
            threshold_prompt_tokens: None,
            keep_recent_n: 8,
        }
    }
}

impl Compactor {
    /// Create a new compactor with the given threshold and default `keep_recent_n` (8).
    pub fn new(threshold_chars: usize) -> Self {
        Self {
            threshold_chars,
            threshold_prompt_tokens: None,
            keep_recent_n: 8,
        }
    }

    /// Set the token-count threshold for compaction.
    ///
    /// When the last turn's `prompt_tokens` (from the API response) meets or
    /// exceeds this value, compaction is triggered instead of relying on the
    /// character-count estimate. Prefer this for non-English workloads.
    pub fn threshold_prompt_tokens(mut self, n: u32) -> Self {
        self.threshold_prompt_tokens = Some(n);
        self
    }

    /// Set the number of recent messages to preserve verbatim.
    pub fn keep_recent_n(mut self, n: usize) -> Self {
        self.keep_recent_n = n;
        self
    }

    /// Estimate the prompt character count of a transcript.
    ///
    /// This is a rough proxy for token count. The agent uses this to decide
    /// whether compaction is needed before the next LLM call.
    /// Includes tool_calls arguments and reasoning_content which were previously
    /// omitted, causing the threshold to be systematically underestimated.
    pub fn estimate_chars(transcript: &[Message]) -> usize {
        transcript
            .iter()
            .map(|m| {
                m.content.len()
                    + m.tool_calls
                        .iter()
                        .map(|tc| tc.name.len() + tc.arguments.to_string().len() + 32)
                        .sum::<usize>()
                    + m.reasoning_content.as_deref().map_or(0, |r| r.len())
            })
            .sum()
    }

    /// JSON schema for structured compaction output.
    const COMPACT_SCHEMA: &'static str = r#"{"type":"object","properties":{"summary":{"type":"string","description":"1-3 paragraph summary of the conversation so far, preserving key decisions, file paths touched, and outcomes."},"kept_facts":{"type":"array","items":{"type":"string"},"description":"Discrete facts worth remembering across compaction (e.g. 'goal=add_X_to_Y', 'compaction happened at step N', 'tool X failed 3 times')."},"next_steps":{"type":"array","items":{"type":"string"},"description":"Outstanding TODOs the agent identified before compaction (each one a single-sentence imperative)."}},"required":["summary","kept_facts"]}"#;

    /// Render a structured compaction result into the message format.
    fn render_structured(
        summary: &str,
        kept_facts: &[String],
        next_steps: &[String],
        step: usize,
    ) -> String {
        let mut rendered = format!(
            "[compacted: structured at step {step}]\n\nSummary: {summary}\n\nKey facts to remember:\n"
        );
        for fact in kept_facts {
            rendered.push_str(&format!("- {fact}\n"));
        }
        if !next_steps.is_empty() {
            rendered.push_str("\nOutstanding TODOs:\n");
            for step in next_steps {
                rendered.push_str(&format!("- {step}\n"));
            }
        }
        rendered
    }

    /// Try structured compaction, returning the rendered string on success.
    /// Returns None if the provider doesn't support it or the response is invalid.
    async fn try_structured_compact(
        &self,
        provider: &dyn ChatProvider,
        older_text: &str,
        step: usize,
    ) -> Option<String> {
        let structured_prompt = format!(
            "Summarize the following conversation. \
             Preserve: file paths modified, key technical decisions, test \
             outcomes, and any errors not yet resolved. Drop: file contents, \
             repeated tool errors, exploratory dead-ends.\n\n\
             Conversation to summarize:\n{older_text}"
        );

        let schema = match serde_json::from_str(Self::COMPACT_SCHEMA) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(error = %e, "COMPACT_SCHEMA is invalid JSON, falling back");
                return None;
            }
        };
        let structured_req = StructuredRequest {
            messages: vec![Message::user(structured_prompt)],
            schema,
            schema_name: "compaction_result".to_string(),
        };

        let json_val = match provider.complete_structured(structured_req).await {
            Ok(v) => v,
            Err(e) => {
                tracing::info!(error = %e, "structured compaction not available, falling back to free-text");
                return None;
            }
        };

        let obj = match json_val.as_object() {
            Some(o) => o,
            None => {
                tracing::warn!(
                    "structured compaction returned non-object, falling back to free-text"
                );
                return None;
            }
        };

        let summary = match obj.get("summary").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => {
                tracing::warn!(
                    "structured compaction missing 'summary' field, falling back to free-text"
                );
                return None;
            }
        };

        let kept_facts: Vec<String> = obj
            .get("kept_facts")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let next_steps: Vec<String> = obj
            .get("next_steps")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        Some(Self::render_structured(
            &summary,
            &kept_facts,
            &next_steps,
            step,
        ))
    }

    /// Compute the safe split point for compaction: the index at which to
    /// divide "older messages to summarize" from "recent messages to keep".
    ///
    /// The retained ("kept") segment must start with a User (or System)
    /// message, because OpenAI/Anthropic require that the first non-System
    /// message after the compaction summary is a User message.  Two cases
    /// can violate this invariant:
    ///
    /// 1. The split point lands on a `Tool` result message — backing up
    ///    preserves tool-call / tool-result pairs.
    /// 2. The split point lands on an `Assistant` message that carries
    ///    `tool_calls` — backing up avoids starting the kept segment with
    ///    an Assistant-with-tool-calls, which would produce an invalid
    ///    `[System(summary), Assistant(tool_calls), Tool, …]` sequence.
    pub fn safe_split_point(transcript: &[Message], keep_n: usize) -> usize {
        let mut split = transcript.len().saturating_sub(keep_n);
        loop {
            if split == 0 || split >= transcript.len() {
                break;
            }
            let msg = &transcript[split];
            let should_back_up = msg.role == crate::message::Role::Tool
                || (msg.role == crate::message::Role::Assistant && !msg.tool_calls.is_empty());
            if should_back_up {
                split -= 1;
            } else {
                break;
            }
        }
        split
    }

    /// Apply compaction in-place to `transcript`, returning `(removed, summary_chars)`.
    ///
    /// Finds the correct split point (never splits inside a tool-call pair),
    /// calls `compact()` to get the summary message, splices the transcript,
    /// and returns how many messages were removed and the summary char count.
    ///
    /// Returns `None` when the transcript is too short to compact
    /// (`< keep_recent_n + 2` messages).
    pub async fn apply_to_transcript(
        &self,
        provider: &dyn ChatProvider,
        transcript: &mut Vec<Message>,
        step: usize,
    ) -> Result<Option<(usize, usize)>> {
        if transcript.len() < self.keep_recent_n + 2 {
            return Ok(None);
        }
        let summary_msg = self.compact(provider, transcript, step).await?;
        let summary_chars = summary_msg.content.len();
        let split = Self::safe_split_point(transcript, self.keep_recent_n);
        let removed = split;
        transcript.drain(..split);
        transcript.insert(0, summary_msg);
        Ok(Some((removed, summary_chars)))
    }

    /// Compact the transcript: summarize older messages into a single system
    /// message, keeping the last `keep_recent_n` messages verbatim.
    ///
    /// `step` is the current turn number and is embedded in the compaction
    /// header for debuggability.
    ///
    /// Returns the summary `Message` that should replace the older portion.
    /// The caller is responsible for splicing it into the transcript.
    #[tracing::instrument(skip(self, provider, transcript))]
    pub async fn compact(
        &self,
        provider: &dyn ChatProvider,
        transcript: &[Message],
        step: usize,
    ) -> Result<Message> {
        let split = Self::safe_split_point(transcript, self.keep_recent_n);
        let older = &transcript[..split];
        let _recent = &transcript[split..];

        // Build a meta-prompt asking the model to summarize the older portion.
        // Include tool_calls on assistant messages so the compactor sees what
        // tools were invoked, not just the text content.
        let older_text: String = older
            .iter()
            .map(|m| {
                let role_tag = match m.role {
                    crate::message::Role::System => "system",
                    crate::message::Role::User => "user",
                    crate::message::Role::Assistant => "assistant",
                    crate::message::Role::Tool => "tool",
                };
                let mut body = m.content.replace('<', "&lt;").replace('>', "&gt;");
                if !m.tool_calls.is_empty() {
                    let calls_summary: Vec<String> = m
                        .tool_calls
                        .iter()
                        .map(|tc| format!("{}({})", tc.name, tc.arguments))
                        .collect();
                    body.push_str(&format!("\n[tool_calls: {}]", calls_summary.join(", ")));
                }
                format!("<{role_tag}>{body}</{role_tag}>")
            })
            .collect::<Vec<_>>()
            .join("\n");

        // Try structured output first
        let summary = match self
            .try_structured_compact(provider, &older_text, step)
            .await
        {
            Some(rendered) => rendered,
            None => {
                // Fall back to free-text path
                let summary_prompt = format!(
                    "Summarize the following conversation in ≤300 words. \
                     Preserve: file paths modified, key technical decisions, test \
                     outcomes, and any errors not yet resolved. Drop: file contents, \
                     repeated tool errors, exploratory dead-ends.\n\n\
                     Conversation to summarize:\n{older_text}"
                );
                let completion = provider
                    .complete(&[Message::user(summary_prompt)], &[] as &[ToolSpec])
                    .await?;
                completion.content
            }
        };

        let summary_chars = summary.len();

        let header = format!(
            "[compacted: {} messages → {} chars at step {step}]\n{}",
            older.len(),
            summary_chars,
            summary
        );

        Ok(Message::system(header).with_compaction_summary())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Completion, MockProvider};

    #[tokio::test]
    async fn compact_returns_system_message_with_summary() {
        let provider = MockProvider::new(vec![Completion {
            content: "Key decisions: added adder tool. Tests pass.".to_string(),
            tool_calls: vec![],
            finish_reason: Some("stop".to_string()),
            usage: None,
            reasoning_content: None,
        }]);

        let transcript = vec![
            Message::system("You are a coding agent.".to_string()),
            Message::user("Add an adder tool".to_string()),
            Message::assistant("Let me create the tool.".to_string()),
            Message::user("Done. Now test it.".to_string()),
            Message::assistant("Tests pass.".to_string()),
        ];

        let compactor = Compactor::new(200).keep_recent_n(2);
        let summary_msg = compactor.compact(&provider, &transcript, 0).await.unwrap();

        assert_eq!(summary_msg.role, crate::message::Role::System);
        assert!(
            summary_msg.is_compaction_summary,
            "compactor must mark summary message with the bit"
        );
        assert!(
            summary_msg.content.contains("[compacted:"),
            "compactor must still include the [compacted: header for debuggability"
        );
        assert!(summary_msg.content.contains("Key decisions:"));
        assert!(summary_msg.content.contains("Tests pass."));
    }

    #[tokio::test]
    async fn compact_preserves_recent_messages() {
        let provider = MockProvider::new(vec![Completion {
            content: "Summary of older messages.".to_string(),
            tool_calls: vec![],
            finish_reason: Some("stop".to_string()),
            usage: None,
            reasoning_content: None,
        }]);

        let transcript = vec![
            Message::system("sys".to_string()),
            Message::user("old goal".to_string()),
            Message::assistant("old reply".to_string()),
            Message::user("recent goal".to_string()),
            Message::assistant("recent reply".to_string()),
        ];

        // keep_recent_n=2 should keep the last 2 messages verbatim
        let compactor = Compactor::new(100).keep_recent_n(2);
        let summary_msg = compactor.compact(&provider, &transcript, 5).await.unwrap();

        assert!(summary_msg.content.contains("[compacted: 3 messages →"));
        // The summary should mention the older messages
        assert!(summary_msg.content.contains("Summary of older messages."));
    }

    #[tokio::test]
    async fn compact_handles_empty_older_portion() {
        let provider = MockProvider::new(vec![Completion {
            content: "nothing to summarize".to_string(),
            tool_calls: vec![],
            finish_reason: Some("stop".to_string()),
            usage: None,
            reasoning_content: None,
        }]);

        let transcript = vec![Message::user("only message".to_string())];

        // keep_recent_n=5 means all messages are "recent", none to compact
        let compactor = Compactor::new(100).keep_recent_n(5);
        let summary_msg = compactor.compact(&provider, &transcript, 0).await.unwrap();

        // Should still produce a summary (even if older portion is empty-ish)
        assert_eq!(summary_msg.role, crate::message::Role::System);
        assert!(
            summary_msg.is_compaction_summary,
            "compactor must mark summary message with the bit"
        );
        assert!(
            summary_msg.content.contains("[compacted:"),
            "compactor must still include the [compacted: header for debuggability"
        );
    }

    #[test]
    fn estimate_chars_sums_content_lengths() {
        let transcript = vec![
            Message::user("hello".to_string()),
            Message::assistant("world".to_string()),
        ];
        assert_eq!(Compactor::estimate_chars(&transcript), 10);
    }

    #[test]
    fn default_threshold_is_max() {
        let c = Compactor::default();
        assert_eq!(c.threshold_chars, usize::MAX);
        assert_eq!(c.threshold_prompt_tokens, None);
        assert_eq!(c.keep_recent_n, 8);
    }

    #[test]
    fn builder_methods_work() {
        let c = Compactor::new(500).keep_recent_n(4);
        assert_eq!(c.threshold_chars, 500);
        assert_eq!(c.threshold_prompt_tokens, None);
        assert_eq!(c.keep_recent_n, 4);
    }

    #[test]
    fn threshold_prompt_tokens_setter_works() {
        let c = Compactor::new(500_000).threshold_prompt_tokens(144_000);
        assert_eq!(c.threshold_chars, 500_000);
        assert_eq!(c.threshold_prompt_tokens, Some(144_000));
        assert_eq!(c.keep_recent_n, 8);
    }

    // ========================================================================
    // Structured compaction tests
    // ========================================================================

    #[tokio::test]
    async fn compactor_structured_happy_path() {
        let json = serde_json::json!({
            "summary": "Added adder tool and verified tests pass.",
            "kept_facts": [
                "goal=add_adder_tool",
                "tool adder created successfully",
                "tests pass"
            ],
            "next_steps": [
                "Add subtractor tool",
                "Run integration tests"
            ]
        });
        let provider = MockProvider::new(vec![]).with_structured_responses(vec![Ok(json)]);

        let transcript = vec![
            Message::system("You are a coding agent.".to_string()),
            Message::user("Add an adder tool".to_string()),
            Message::assistant("Let me create the tool.".to_string()),
            Message::user("Done. Now test it.".to_string()),
            Message::assistant("Tests pass.".to_string()),
        ];

        let compactor = Compactor::new(200).keep_recent_n(2);
        let summary_msg = compactor.compact(&provider, &transcript, 3).await.unwrap();

        assert_eq!(summary_msg.role, crate::message::Role::System);
        // Should contain the structured rendering format with the step number
        assert!(summary_msg
            .content
            .contains("[compacted: structured at step 3]"));
        assert!(summary_msg.content.contains("Summary: Added adder tool"));
        assert!(summary_msg.content.contains("Key facts to remember:"));
        assert!(summary_msg.content.contains("- goal=add_adder_tool"));
        assert!(summary_msg
            .content
            .contains("- tool adder created successfully"));
        assert!(summary_msg.content.contains("- tests pass"));
        assert!(summary_msg.content.contains("Outstanding TODOs:"));
        assert!(summary_msg.content.contains("- Add subtractor tool"));
        assert!(summary_msg.content.contains("- Run integration tests"));
    }

    #[tokio::test]
    async fn compactor_falls_back_on_structured_error() {
        // MockProvider with no structured responses configured -> returns error
        let provider = MockProvider::new(vec![Completion {
            content: "Free-text fallback summary.".to_string(),
            tool_calls: vec![],
            finish_reason: Some("stop".to_string()),
            usage: None,
            reasoning_content: None,
        }]);

        let transcript = vec![
            Message::user("goal".to_string()),
            Message::assistant("response".to_string()),
        ];

        let compactor = Compactor::new(100).keep_recent_n(1);
        let summary_msg = compactor.compact(&provider, &transcript, 0).await.unwrap();

        assert_eq!(summary_msg.role, crate::message::Role::System);
        // Should have fallen back to free-text format
        assert!(
            summary_msg.is_compaction_summary,
            "compactor must mark summary message with the bit"
        );
        assert!(
            summary_msg.content.contains("[compacted:"),
            "compactor must still include the [compacted: header for debuggability"
        );
        assert!(summary_msg.content.contains("Free-text fallback summary."));
    }

    #[tokio::test]
    async fn compactor_structured_invalid_response_falls_back() {
        // Return valid JSON but not matching the schema (missing 'summary')
        let json = serde_json::json!({
            "foo": "bar"
        });
        let provider = MockProvider::new(vec![Completion {
            content: "Fallback after invalid structured response.".to_string(),
            tool_calls: vec![],
            finish_reason: Some("stop".to_string()),
            usage: None,
            reasoning_content: None,
        }])
        .with_structured_responses(vec![Ok(json)]);

        let transcript = vec![
            Message::user("goal".to_string()),
            Message::assistant("response".to_string()),
        ];

        let compactor = Compactor::new(100).keep_recent_n(1);
        let summary_msg = compactor.compact(&provider, &transcript, 0).await.unwrap();

        assert_eq!(summary_msg.role, crate::message::Role::System);
        // Should have fallen back to free-text format
        assert!(
            summary_msg.is_compaction_summary,
            "compactor must mark summary message with the bit"
        );
        assert!(
            summary_msg.content.contains("[compacted:"),
            "compactor must still include the [compacted: header for debuggability"
        );
        assert!(summary_msg
            .content
            .contains("Fallback after invalid structured response."));
    }

    #[test]
    fn safe_split_point_keep_n_zero_does_not_panic() {
        // keep_n=0 means split = transcript.len(); the while loop must not
        // index transcript[transcript.len()] — that would be out of bounds.
        let msgs = vec![
            Message::user("a".to_string()),
            Message::assistant("b".to_string()),
        ];
        let split = Compactor::safe_split_point(&msgs, 0);
        assert_eq!(split, msgs.len(), "keep_n=0 should return full length");
    }

    #[test]
    fn safe_split_point_empty_transcript_does_not_panic() {
        let split = Compactor::safe_split_point(&[], 0);
        assert_eq!(split, 0);
        let split = Compactor::safe_split_point(&[], 5);
        assert_eq!(split, 0);
    }

    #[test]
    fn safe_split_backs_up_past_tool_and_assistant_with_tool_calls() {
        // Sequence: [user, asst+tc, tool_result, user, asst]  (indices 0..4)
        // keep_n=3: initial split = 5-3 = 2 (Tool message)
        //   → backs up to 1 (Assistant+tool_calls)
        //   → backs up to 0 (loop breaks: split==0)
        use crate::llm::ToolCall;
        let asst_with_calls = Message::assistant_with_tool_calls(
            "thinking".to_string(),
            vec![ToolCall {
                id: "call_1".into(),
                name: "Read".into(),
                arguments: serde_json::json!("{}"),
            }],
        );
        let msgs = vec![
            Message::user("start".to_string()),
            asst_with_calls,
            Message::tool_result("call_1", "file contents"),
            Message::user("continue".to_string()),
            Message::assistant("done".to_string()),
        ];
        let split = Compactor::safe_split_point(&msgs, 3);
        assert_eq!(
            split, 0,
            "should back up past both Tool and Assistant-with-tool-calls to index 0"
        );
    }

    #[test]
    fn safe_split_backs_up_when_landing_directly_on_assistant_with_tool_calls() {
        // Sequence: [user, asst+tc, user, asst]  (indices 0..3)
        // keep_n=3: initial split = 4-3 = 1 (Assistant+tool_calls)
        //   → backs up to 0 (loop breaks: split==0)
        use crate::llm::ToolCall;
        let asst_with_calls = Message::assistant_with_tool_calls(
            "planning".to_string(),
            vec![ToolCall {
                id: "c1".into(),
                name: "Glob".into(),
                arguments: serde_json::json!("{}"),
            }],
        );
        let msgs = vec![
            Message::user("first question".to_string()),
            asst_with_calls,
            Message::user("follow-up".to_string()),
            Message::assistant("answer".to_string()),
        ];
        let split = Compactor::safe_split_point(&msgs, 3);
        assert_eq!(
            split, 0,
            "split landing directly on Assistant+tool_calls should retreat to 0"
        );
    }

    #[test]
    fn safe_split_no_backup_when_split_is_already_valid() {
        // Sequence: [user, asst+tc, tool_result, user, asst]  (indices 0..4)
        // keep_n=2: initial split = 5-2 = 3 (User message) → no backup needed
        use crate::llm::ToolCall;
        let asst_with_calls = Message::assistant_with_tool_calls(
            "".to_string(),
            vec![ToolCall {
                id: "x".into(),
                name: "Write".into(),
                arguments: serde_json::json!("{}"),
            }],
        );
        let msgs = vec![
            Message::user("start".to_string()),
            asst_with_calls,
            Message::tool_result("x", "ok"),
            Message::user("continue".to_string()),
            Message::assistant("done".to_string()),
        ];
        let split = Compactor::safe_split_point(&msgs, 2);
        assert_eq!(split, 3, "split at a User message should not retreat");
    }

    // ========================================================================
    // estimate_chars — tool_calls and reasoning_content coverage
    // ========================================================================

    #[test]
    fn estimate_chars_includes_tool_calls() {
        use crate::llm::ToolCall;
        // An assistant message with one tool call.
        // name = "Read" (4 chars), arguments = "\"path\"" (6 chars), overhead = 32
        let tool_call = ToolCall {
            id: "c1".into(),
            name: "Read".into(),
            arguments: serde_json::json!("path"),
        };
        let msg = Message::assistant_with_tool_calls("think".to_string(), vec![tool_call]);
        // content=5, name=4, args=serde representation len, overhead=32
        let args_len = msg.tool_calls[0].arguments.to_string().len();
        let expected = 5 + (4 + args_len + 32);
        assert_eq!(
            Compactor::estimate_chars(&[msg]),
            expected,
            "tool_calls contribution must be counted"
        );
    }

    #[test]
    fn estimate_chars_includes_reasoning_content() {
        let mut msg = Message::assistant("hello".to_string());
        msg.reasoning_content = Some("some reasoning".to_string());
        // content=5, reasoning=14
        assert_eq!(
            Compactor::estimate_chars(&[msg]),
            5 + 14,
            "reasoning_content must be counted"
        );
    }

    #[test]
    fn estimate_chars_tool_calls_plus_reasoning_combined() {
        use crate::llm::ToolCall;
        let tool_call = ToolCall {
            id: "c2".into(),
            name: "Write".into(),
            arguments: serde_json::json!({"path": "x"}),
        };
        let mut msg = Message::assistant_with_tool_calls("".to_string(), vec![tool_call]);
        msg.reasoning_content = Some("reason".to_string());
        let args_len = msg.tool_calls[0].arguments.to_string().len();
        // content=0, tool_call: name=5, args=..., +32; reasoning=6
        let expected = (5 + args_len + 32) + 6;
        assert_eq!(Compactor::estimate_chars(&[msg]), expected);
    }

    // ========================================================================
    // apply_to_transcript — splice and return-value coverage
    // ========================================================================

    #[tokio::test]
    async fn apply_to_transcript_splices_and_returns_counts() {
        let provider = MockProvider::new(vec![Completion {
            content: "summary of first three".to_string(),
            tool_calls: vec![],
            finish_reason: Some("stop".to_string()),
            usage: None,
            reasoning_content: None,
        }]);

        let mut transcript = vec![
            Message::system("sys".to_string()),
            Message::user("msg1".to_string()),
            Message::assistant("rep1".to_string()),
            Message::user("msg2".to_string()),
            Message::assistant("rep2".to_string()),
            Message::user("msg3".to_string()),
        ];

        // keep_recent_n=2 → split=4, compact first 4 msgs → removed=4
        let compactor = Compactor::new(50).keep_recent_n(2);
        let result = compactor
            .apply_to_transcript(&provider, &mut transcript, 5)
            .await
            .unwrap();

        // Should have returned Some((removed, summary_chars))
        let (removed, summary_chars) =
            result.expect("should compact when transcript is long enough");
        assert!(removed > 0, "removed must be > 0 when compaction ran");
        assert!(summary_chars > 0, "summary_chars must be > 0");

        // Transcript must start with the compaction summary
        assert_eq!(transcript[0].role, crate::message::Role::System);
        assert!(transcript[0].is_compaction_summary);
        // The 2 recent messages are preserved after the summary
        assert!(transcript.len() >= 3, "summary + at least 2 recent");
    }

    #[tokio::test]
    async fn apply_to_transcript_too_short_returns_none() {
        // A provider that should never be called
        let provider = MockProvider::new(vec![]);

        let mut transcript = vec![
            Message::user("only one".to_string()),
            Message::assistant("reply".to_string()),
        ];

        // keep_recent_n=8 + 2 = 10 required, but transcript has only 2
        let compactor = Compactor::new(50).keep_recent_n(8);
        let result = compactor
            .apply_to_transcript(&provider, &mut transcript, 0)
            .await
            .unwrap();

        assert!(result.is_none(), "transcript too short must return None");
        // Transcript must not be modified
        assert_eq!(transcript.len(), 2);
    }

    #[tokio::test]
    async fn compact_includes_tool_calls_in_older_text() {
        // Verify that assistant messages carrying tool_calls contribute their
        // call summaries to the text sent to the LLM for summarization.
        // If the `!m.tool_calls.is_empty()` guard is inverted, the calls
        // are omitted (or spurious empty `[tool_calls: ]` is appended to
        // messages that have no calls), which breaks the compactor's ability
        // to preserve tool-invocation context across compaction boundaries.
        use crate::llm::{Completion, MockProvider, ToolCall};
        let provider = MockProvider::new(vec![Completion {
            content: "summary".to_string(),
            tool_calls: vec![],
            finish_reason: Some("stop".to_string()),
            usage: None,
            reasoning_content: None,
        }]);

        let tool_call = ToolCall {
            id: "c1".into(),
            name: "Read".into(),
            arguments: serde_json::json!({"path": "src/lib.rs"}),
        };
        let asst_with_calls =
            Message::assistant_with_tool_calls("thinking".to_string(), vec![tool_call]);
        // Build transcript long enough that the tool-call pair ends up in
        // the "older" portion: [user, asst+tc, tool_result, user] are older,
        // [asst, user] are "recent" (keep_recent_n=2).
        // safe_split_point with keep_n=2 on 6 msgs: initial split=4
        // (transcript[4] = "asst" — no tool_calls, no Tool role) → no retreat.
        let transcript = vec![
            Message::user("start".to_string()),          // [0] older
            asst_with_calls,                             // [1] older  ← has tool_calls
            Message::tool_result("c1", "file contents"), // [2] older
            Message::user("continue".to_string()),       // [3] older
            Message::assistant("done".to_string()),      // [4] recent
            Message::user("next".to_string()),           // [5] recent
        ];

        let compactor = Compactor::new(0).keep_recent_n(2);
        let _ = compactor.compact(&provider, &transcript, 0).await.unwrap();

        let calls = provider.calls();
        assert!(!calls.is_empty(), "provider must have been called");
        // The first (and only) call is the summarization request.
        let prompt_msg = &calls[0][0];
        assert!(
            prompt_msg.content.contains("Read"),
            "older_text must include the tool call name 'Read'; got: {}",
            prompt_msg.content
        );
        assert!(
            prompt_msg.content.contains("[tool_calls:"),
            "older_text must include the [tool_calls: ...] marker; got: {}",
            prompt_msg.content
        );
    }

    #[tokio::test]
    async fn apply_to_transcript_minimum_length_boundary() {
        // Exactly at the boundary: keep_recent_n + 2 messages → should compact.
        let provider = MockProvider::new(vec![Completion {
            content: "boundary summary".to_string(),
            tool_calls: vec![],
            finish_reason: Some("stop".to_string()),
            usage: None,
            reasoning_content: None,
        }]);

        let keep_n = 2_usize;
        // Minimum length = keep_n + 2 = 4
        let mut transcript = vec![
            Message::system("s".to_string()),
            Message::user("u".to_string()),
            Message::assistant("a".to_string()),
            Message::user("u2".to_string()),
        ];

        let compactor = Compactor::new(0).keep_recent_n(keep_n);
        let result = compactor
            .apply_to_transcript(&provider, &mut transcript, 1)
            .await
            .unwrap();

        assert!(
            result.is_some(),
            "should compact when len == keep_recent_n + 2"
        );
    }

    // ========================================================================
    // render_structured — direct unit tests
    // ========================================================================

    #[test]
    fn render_structured_with_next_steps_includes_todos_section() {
        let rendered = Compactor::render_structured(
            "summary text",
            &["fact A".into(), "fact B".into()],
            &["step 1".into()],
            5,
        );
        assert!(rendered.contains("[compacted: structured at step 5]"));
        assert!(rendered.contains("Summary: summary text"));
        assert!(rendered.contains("- fact A"));
        assert!(rendered.contains("- fact B"));
        assert!(
            rendered.contains("Outstanding TODOs:"),
            "non-empty next_steps must show TODOs section"
        );
        assert!(rendered.contains("- step 1"));
    }

    #[test]
    fn render_structured_without_next_steps_omits_todos_section() {
        // Kills `delete !` at line 85: if next_steps.is_empty() (mutated from !next_steps.is_empty())
        // then the TODO block would appear even for empty next_steps.
        let rendered = Compactor::render_structured(
            "summary",
            &["fact".into()],
            &[], // empty next_steps
            1,
        );
        assert!(
            !rendered.contains("Outstanding TODOs:"),
            "empty next_steps must NOT include TODOs section (kills delete-! mutation)"
        );
    }

    #[tokio::test]
    async fn apply_to_transcript_boundary_plus_not_times() {
        // Kills: `replace + with * in Compactor::apply_to_transcript` at line 225.
        //
        // The threshold is `keep_recent_n + 2`, NOT `keep_recent_n * 2`.
        // At keep_recent_n=2, both formulas yield 4, so we need keep_recent_n != 2.
        //
        // Use keep_recent_n=3 (threshold = 3+2=5, but 3*2=6).
        // With transcript.len() == 5:
        //   original `+`: 5 < 5 → false → compaction fires
        //   mutant `*`:   5 < 6 → true  → early return, no compaction
        let provider = MockProvider::new(vec![Completion {
            content: "summary".to_string(),
            tool_calls: vec![],
            finish_reason: Some("stop".to_string()),
            usage: None,
            reasoning_content: None,
        }]);

        let keep_n = 3_usize;
        // Exactly keep_n + 2 = 5 messages
        let mut transcript = vec![
            Message::system("s0".to_string()),
            Message::user("u1".to_string()),
            Message::assistant("a1".to_string()),
            Message::user("u2".to_string()),
            Message::assistant("a2".to_string()),
        ];
        assert_eq!(
            transcript.len(),
            keep_n + 2,
            "test setup: must be exactly keep_n + 2 messages"
        );

        let compactor = Compactor::new(0).keep_recent_n(keep_n);
        let result = compactor
            .apply_to_transcript(&provider, &mut transcript, 1)
            .await
            .unwrap();

        assert!(
            result.is_some(),
            "must compact when len == keep_recent_n + 2 (threshold uses `+`, not `*`)"
        );
    }
}
