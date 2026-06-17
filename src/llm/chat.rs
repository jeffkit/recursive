//! Chat completion data types.
//!
//! These types are the wire contract between the [`ChatProvider`] trait and
//! its callers. All provider implementations produce and consume these
//! shapes; nothing in `tools/` or `agent/` depends on provider internals.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::message::Message;

/// Channel sender for streaming partial tokens during a streaming LLM call.
/// Each `String` is a delta chunk (partial token) emitted by the provider.
pub type StreamSender = mpsc::UnboundedSender<String>;

/// Token usage data from an LLM response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
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
