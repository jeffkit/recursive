//! LLM provider abstraction.
//!
//! A provider takes a transcript plus tool specs and returns either
//! free-form content, structured tool calls, or both. The trait is the
//! only thing the agent depends on; everything beyond it (HTTP, retries,
//! mocking) lives in adapters.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Result;
use crate::message::Message;

pub mod mock;
pub mod openai;

pub use mock::MockProvider;
pub use openai::OpenAiProvider;

/// Token usage data from an LLM response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

impl TokenUsage {
    /// Saturating element-wise sum. Used to accumulate across LLM calls.
    pub fn accumulate(self, other: TokenUsage) -> TokenUsage {
        TokenUsage {
            prompt_tokens: self.prompt_tokens.saturating_add(other.prompt_tokens),
            completion_tokens: self
                .completion_tokens
                .saturating_add(other.completion_tokens),
            total_tokens: self.total_tokens.saturating_add(other.total_tokens),
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON arguments as produced by the model.
    pub arguments: Value,
}

/// One step of model output.
#[derive(Debug, Clone)]
pub struct Completion {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub usage: Option<TokenUsage>,
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn complete(&self, messages: &[Message], tools: &[ToolSpec]) -> Result<Completion>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_usage_default_is_all_zeros() {
        let u = TokenUsage::default();
        assert_eq!(u.prompt_tokens, 0);
        assert_eq!(u.completion_tokens, 0);
        assert_eq!(u.total_tokens, 0);
    }

    #[test]
    fn token_usage_accumulate_is_saturating() {
        let u1 = TokenUsage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
        };
        let u2 = TokenUsage {
            prompt_tokens: 20,
            completion_tokens: 30,
            total_tokens: 50,
        };
        let acc = u1.accumulate(u2);
        assert_eq!(acc.prompt_tokens, 30);
        assert_eq!(acc.completion_tokens, 35);
        assert_eq!(acc.total_tokens, 65);
    }

    #[test]
    fn token_usage_accumulate_is_commutative() {
        let u1 = TokenUsage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
        };
        let u2 = TokenUsage {
            prompt_tokens: 20,
            completion_tokens: 30,
            total_tokens: 50,
        };
        assert_eq!(u1.accumulate(u2), u2.accumulate(u1));
    }

    #[test]
    fn token_usage_accumulate_saturates() {
        let u1 = TokenUsage {
            prompt_tokens: u32::MAX,
            completion_tokens: 1,
            total_tokens: u32::MAX,
        };
        let u2 = TokenUsage {
            prompt_tokens: 1,
            completion_tokens: u32::MAX,
            total_tokens: u32::MAX,
        };
        let acc = u1.accumulate(u2);
        assert_eq!(acc.prompt_tokens, u32::MAX);
        assert_eq!(acc.completion_tokens, u32::MAX);
        assert_eq!(acc.total_tokens, u32::MAX);
    }
}
