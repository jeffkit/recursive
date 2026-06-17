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
    pub threshold_chars: usize,
    /// Number of most-recent messages to keep verbatim during compaction.
    pub keep_recent_n: usize,
}

impl Default for Compactor {
    fn default() -> Self {
        Self {
            threshold_chars: usize::MAX,
            keep_recent_n: 8,
        }
    }
}

impl Compactor {
    /// Create a new compactor with the given threshold and default `keep_recent_n` (8).
    pub fn new(threshold_chars: usize) -> Self {
        Self {
            threshold_chars,
            keep_recent_n: 8,
        }
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
    /// Never splits in the middle of a Tool role message — if the natural
    /// split point lands on a Tool message, it backs up until it finds a
    /// non-Tool boundary, preserving tool-call / tool-result pairs.
    pub fn safe_split_point(transcript: &[Message], keep_n: usize) -> usize {
        let mut split = transcript.len().saturating_sub(keep_n);
        while split > 0 && matches!(transcript[split].role, crate::message::Role::Tool) {
            split -= 1;
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

        let _older_chars: usize = older.iter().map(|m| m.content.len()).sum();
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
        assert_eq!(c.keep_recent_n, 8);
    }

    #[test]
    fn builder_methods_work() {
        let c = Compactor::new(500).keep_recent_n(4);
        assert_eq!(c.threshold_chars, 500);
        assert_eq!(c.keep_recent_n, 4);
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
}
