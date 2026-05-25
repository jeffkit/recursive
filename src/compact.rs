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
use crate::llm::{LlmProvider, ToolSpec};
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
    pub fn estimate_chars(transcript: &[Message]) -> usize {
        transcript.iter().map(|m| m.content.len()).sum()
    }

    /// Compact the transcript: summarize older messages into a single system
    /// message, keeping the last `keep_recent_n` messages verbatim.
    ///
    /// Returns the summary `Message` that should replace the older portion.
    /// The caller is responsible for splicing it into the transcript.
    #[tracing::instrument(skip(self, provider, transcript))]
    pub async fn compact(
        &self,
        provider: &dyn LlmProvider,
        transcript: &[Message],
    ) -> Result<Message> {
        let n = self.keep_recent_n.min(transcript.len().saturating_sub(1));
        let split = transcript.len().saturating_sub(n);
        let older = &transcript[..split];
        let _recent = &transcript[split..];

        // Build a meta-prompt asking the model to summarize the older portion.
        let older_text: String = older
            .iter()
            .map(|m| {
                let role_tag = match m.role {
                    crate::message::Role::System => "system",
                    crate::message::Role::User => "user",
                    crate::message::Role::Assistant => "assistant",
                    crate::message::Role::Tool => "tool",
                };
                format!("<{role_tag}>{}</{role_tag}>", m.content)
            })
            .collect::<Vec<_>>()
            .join("\n");

        let summary_prompt = format!(
            "Summarize the following conversation in ≤300 words. \
             Preserve: file paths modified, key technical decisions, test \
             outcomes, and any errors not yet resolved. Drop: file contents, \
             repeated tool errors, exploratory dead-ends.\n\n\
             Conversation to summarize:\n{older_text}"
        );

        let summary_messages = vec![Message::user(summary_prompt)];
        let completion = provider
            .complete(&summary_messages, &[] as &[ToolSpec])
            .await?;

        let summary = completion.content;
        let _older_chars: usize = older.iter().map(|m| m.content.len()).sum();
        let summary_chars = summary.len();

        let header = format!(
            "[compacted: {} messages → {} chars]\n{}",
            older.len(),
            summary_chars,
            summary
        );

        Ok(Message::system(header))
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
        }]);

        let transcript = vec![
            Message::system("You are a coding agent.".to_string()),
            Message::user("Add an adder tool".to_string()),
            Message::assistant("Let me create the tool.".to_string()),
            Message::user("Done. Now test it.".to_string()),
            Message::assistant("Tests pass.".to_string()),
        ];

        let compactor = Compactor::new(200).keep_recent_n(2);
        let summary_msg = compactor.compact(&provider, &transcript).await.unwrap();

        assert_eq!(summary_msg.role, crate::message::Role::System);
        assert!(summary_msg.content.contains("[compacted:"));
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
        let summary_msg = compactor.compact(&provider, &transcript).await.unwrap();

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
        }]);

        let transcript = vec![Message::user("only message".to_string())];

        // keep_recent_n=5 means all messages are "recent", none to compact
        let compactor = Compactor::new(100).keep_recent_n(5);
        let summary_msg = compactor.compact(&provider, &transcript).await.unwrap();

        // Should still produce a summary (even if older portion is empty-ish)
        assert_eq!(summary_msg.role, crate::message::Role::System);
        assert!(summary_msg.content.contains("[compacted:"));
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
}
