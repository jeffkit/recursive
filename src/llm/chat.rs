//! Chat completion data types.
//!
//! These types are the wire contract between the [`ChatProvider`] trait and
//! its callers. All provider implementations produce and consume these
//! shapes; nothing in `tools/` or `agent/` depends on provider internals.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::message::Message;

/// A single streamed delta emitted by a provider during a streaming LLM call.
///
/// Providers that expose an explicit reasoning channel (DeepSeek R1, OpenAI
/// o1/o3, Anthropic / DeepSeek extended thinking) emit `Reasoning` deltas
/// *before* the answer's `Text` deltas, so consumers can render the
/// chain-of-thought live and above the final answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamChunk {
    /// A delta of the visible answer text.
    Text(String),
    /// A delta of the model's reasoning / thinking text.
    Reasoning(String),
}

/// Channel sender for streaming partial deltas during a streaming LLM call.
/// Each [`StreamChunk`] is a delta emitted by the provider.
pub type StreamSender = mpsc::UnboundedSender<StreamChunk>;

/// Token usage data from an LLM response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    /// Input tokens served from the provider's prompt cache.
    ///
    /// Invariant (normalised across providers):
    /// `cache_hit_tokens + cache_miss_tokens == total input tokens`.
    /// A "hit" is a token read from cache; everything else processed for the
    /// prompt (fresh input *and* tokens written to cache) counts as a "miss".
    /// This lets consumers compute a cache-hit rate as
    /// `cache_hit / (cache_hit + cache_miss)` uniformly, regardless of which
    /// provider reported the usage. DeepSeek already reports the split this
    /// way (`prompt_tokens = hit + miss`); Anthropic reports `input_tokens`,
    /// `cache_read_input_tokens` and `cache_creation_input_tokens` separately,
    /// so its parser folds `input + creation` into `cache_miss_tokens`.
    pub cache_hit_tokens: u32,
    pub cache_miss_tokens: u32,
    /// Reasoning / thinking tokens emitted by models that support
    /// extended thinking (DeepSeek R1, OpenAI o1, Anthropic
    /// extended thinking). Adds to the cost total because the
    /// model spent compute producing them. Default 0 for models
    /// that don't report this separately.
    pub reasoning_tokens: u32,
}

impl TokenUsage {
    /// Saturating element-wise sum. Used to accumulate across LLM calls.
    pub fn accumulate(self, other: TokenUsage) -> TokenUsage {
        TokenUsage {
            reasoning_tokens: self.reasoning_tokens.saturating_add(other.reasoning_tokens),
            prompt_tokens: self.prompt_tokens.saturating_add(other.prompt_tokens),
            completion_tokens: self
                .completion_tokens
                .saturating_add(other.completion_tokens),
            total_tokens: self.total_tokens.saturating_add(other.total_tokens),
            cache_hit_tokens: self.cache_hit_tokens.saturating_add(other.cache_hit_tokens),
            cache_miss_tokens: self
                .cache_miss_tokens
                .saturating_add(other.cache_miss_tokens),
        }
    }
}

/// JSON-schema description of a tool, sent verbatim to the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema object describing the tool's input.
    pub parameters: Value,
}

/// A structured request to invoke one of the registered tools.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON arguments as produced by the model.
    pub arguments: Value,
}

/// Request for a structured JSON response conforming to a JSON schema.
pub struct StructuredRequest {
    pub messages: Vec<Message>,
    /// JSON Schema describing the expected response shape.
    pub schema: Value,
    /// Name for the schema (sent to the provider as `schema_name`).
    pub schema_name: String,
}

/// One step of model output.
#[derive(Debug, Clone, Default)]
pub struct Completion {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub usage: Option<TokenUsage>,
    /// DeepSeek reasoning/thinking content. Stored in the transcript and
    /// echoed back on subsequent requests to satisfy the API contract.
    pub reasoning_content: Option<String>,
}

impl Completion {
    /// Normalise chain-of-thought that a model emitted *inline* in the
    /// `content` field wrapped in `<think>…</think>` tags into the
    /// dedicated [`Completion::reasoning_content`] channel.
    ///
    /// Many DeepSeek-R1 style deployments (and OpenAI-compatible proxies)
    /// don't populate the separate `reasoning_content` SSE field; they
    /// stream the thinking inline as `<think>…</think>` inside `content`.
    /// Downstream the TUI markdown renderer parses those tags as an HTML
    /// block and silently drops the whole section — so the thinking is
    /// visible while streaming but disappears once the turn finalises and
    /// the assistant block is re-rendered from `content`.
    ///
    /// This moves the inner text into `reasoning_content` (so the existing
    /// `AgentEvent::Reasoning` → thinking-block pipeline lights up) and
    /// strips the tags from `content` (so the answer renders cleanly).
    ///
    /// No-op when `reasoning_content` is already populated (true reasoner
    /// models that use the dedicated field) or when no `<think>` tag is
    /// present.
    pub fn extract_inline_reasoning(&mut self) {
        if self
            .reasoning_content
            .as_deref()
            .is_some_and(|r| !r.trim().is_empty())
        {
            return;
        }
        let Some((reasoning, cleaned)) = split_think_tags(&self.content) else {
            return;
        };
        if !reasoning.is_empty() {
            self.reasoning_content = Some(reasoning);
        }
        self.content = cleaned;
    }
}

/// Split a `<think>…</think>` block out of `content`.
///
/// Returns `Some((reasoning, cleaned_content))` when an opening `<think>`
/// tag is present, else `None`. An unclosed `<think>` (e.g. a truncated
/// response) treats everything after the tag as reasoning. Both halves are
/// trimmed of surrounding whitespace.
fn split_think_tags(content: &str) -> Option<(String, String)> {
    const OPEN: &str = "<think>";
    const CLOSE: &str = "</think>";

    let open_idx = content.find(OPEN)?;
    let after_open = open_idx + OPEN.len();

    if let Some(rel_close) = content[after_open..].find(CLOSE) {
        let close_idx = after_open + rel_close;
        let reasoning = content[after_open..close_idx].trim().to_string();
        let mut cleaned = String::with_capacity(content.len());
        cleaned.push_str(&content[..open_idx]);
        cleaned.push_str(&content[close_idx + CLOSE.len()..]);
        Some((reasoning, cleaned.trim().to_string()))
    } else {
        // Unclosed tag: treat the remainder as reasoning.
        let reasoning = content[after_open..].trim().to_string();
        let cleaned = content[..open_idx].trim().to_string();
        Some((reasoning, cleaned))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completion_with_content(content: &str) -> Completion {
        Completion {
            content: content.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn extract_inline_reasoning_moves_think_block_to_reasoning() {
        let mut c = completion_with_content(
            "<think>\nlet me work through this\n</think>\n\nThe answer is 42.",
        );
        c.extract_inline_reasoning();
        assert_eq!(
            c.reasoning_content.as_deref(),
            Some("let me work through this")
        );
        assert_eq!(c.content, "The answer is 42.");
    }

    #[test]
    fn extract_inline_reasoning_handles_inline_single_line() {
        let mut c = completion_with_content("<think>quick thought</think>answer");
        c.extract_inline_reasoning();
        assert_eq!(c.reasoning_content.as_deref(), Some("quick thought"));
        assert_eq!(c.content, "answer");
    }

    #[test]
    fn extract_inline_reasoning_unclosed_tag_treats_rest_as_reasoning() {
        let mut c = completion_with_content("partial answer<think>still thinking");
        c.extract_inline_reasoning();
        assert_eq!(c.reasoning_content.as_deref(), Some("still thinking"));
        assert_eq!(c.content, "partial answer");
    }

    #[test]
    fn extract_inline_reasoning_no_tag_is_noop() {
        let mut c = completion_with_content("just a plain answer");
        c.extract_inline_reasoning();
        assert!(c.reasoning_content.is_none());
        assert_eq!(c.content, "just a plain answer");
    }

    // ── TokenUsage::accumulate tests ─────────────────────────────────────────

    #[test]
    fn token_usage_accumulate_sums_all_fields() {
        // kills function-level replacement and any single-field mutation in accumulate()
        let a = TokenUsage {
            reasoning_tokens: 10,
            prompt_tokens: 20,
            completion_tokens: 30,
            total_tokens: 60,
            cache_hit_tokens: 5,
            cache_miss_tokens: 15,
        };
        let b = TokenUsage {
            reasoning_tokens: 1,
            prompt_tokens: 2,
            completion_tokens: 3,
            total_tokens: 6,
            cache_hit_tokens: 4,
            cache_miss_tokens: 9,
        };
        let c = a.accumulate(b);
        assert_eq!(c.reasoning_tokens, 11);
        assert_eq!(c.prompt_tokens, 22);
        assert_eq!(c.completion_tokens, 33);
        assert_eq!(c.total_tokens, 66);
        assert_eq!(c.cache_hit_tokens, 9);
        assert_eq!(c.cache_miss_tokens, 24);
    }

    #[test]
    fn token_usage_accumulate_saturates_on_overflow() {
        // kills `saturating_add` → wrapping_add or `+` mutations
        let big = TokenUsage {
            reasoning_tokens: u32::MAX,
            prompt_tokens: u32::MAX,
            completion_tokens: u32::MAX,
            total_tokens: u32::MAX,
            cache_hit_tokens: u32::MAX,
            cache_miss_tokens: u32::MAX,
        };
        let one = TokenUsage {
            reasoning_tokens: 1,
            prompt_tokens: 1,
            completion_tokens: 1,
            total_tokens: 1,
            cache_hit_tokens: 1,
            cache_miss_tokens: 1,
        };
        let result = big.accumulate(one);
        assert_eq!(result.reasoning_tokens, u32::MAX, "must saturate, not overflow");
        assert_eq!(result.prompt_tokens, u32::MAX);
        assert_eq!(result.completion_tokens, u32::MAX);
    }

    #[test]
    fn split_think_tags_empty_block_returns_empty_reasoning() {
        // kills mutations in the trimming / boundary logic for empty think blocks
        let result = split_think_tags("<think></think>answer");
        assert!(result.is_some(), "empty think block must still parse");
        let (reasoning, cleaned) = result.unwrap();
        assert!(reasoning.is_empty(), "reasoning must be empty for empty tags; got: {reasoning:?}");
        assert_eq!(cleaned, "answer");
    }

    #[test]
    fn extract_inline_reasoning_preserves_existing_reasoning_field() {
        // True reasoner model: reasoning_content already populated via the
        // dedicated SSE field. Leave content untouched even if it happens
        // to contain a stray tag.
        let mut c = Completion {
            content: "<think>ignored</think>answer".to_string(),
            reasoning_content: Some("real reasoning".to_string()),
            ..Default::default()
        };
        c.extract_inline_reasoning();
        assert_eq!(c.reasoning_content.as_deref(), Some("real reasoning"));
        assert_eq!(c.content, "<think>ignored</think>answer");
    }
}
