//! LLM provider abstraction.
//!
//! A provider takes a transcript plus tool specs and returns either
//! free-form content, structured tool calls, or both. The trait is the
//! only thing the agent depends on; everything beyond it (HTTP, retries,
//! mocking) lives in adapters.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Error;
use crate::error::Result;
use crate::message::Message;

use tokio::sync::mpsc;

pub mod anthropic;
pub mod mock;
pub mod openai;

pub use anthropic::AnthropicProvider;
pub use mock::MockProvider;
pub use openai::OpenAiProvider;

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
            cache_hit_tokens: self.cache_hit_tokens.saturating_add(other.cache_hit_tokens),
            cache_miss_tokens: self
                .cache_miss_tokens
                .saturating_add(other.cache_miss_tokens),
        }
    }
}

/// Per-million-token pricing for one model. USD.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPricing {
    pub input_per_million: f64,
    pub output_per_million: f64,
}

impl ModelPricing {
    /// USD cost for the given usage at this pricing.
    pub fn cost_usd(&self, usage: TokenUsage) -> f64 {
        let in_cost = (usage.prompt_tokens as f64) * self.input_per_million / 1_000_000.0;
        let out_cost = (usage.completion_tokens as f64) * self.output_per_million / 1_000_000.0;
        in_cost + out_cost
    }
}

/// Returns pricing for known models, or None if unknown.
pub fn pricing_for(model: &str) -> Option<ModelPricing> {
    match model {
        "MiniMax-M2" => Some(ModelPricing {
            input_per_million: 0.30,
            output_per_million: 1.20,
        }),
        "deepseek-chat" | "deepseek-v4-flash" => Some(ModelPricing {
            input_per_million: 0.27,
            output_per_million: 1.10,
        }),
        // V4-Pro is ~7× flash on input; placeholder until calibrated
        // against the DeepSeek billing dashboard.
        "deepseek-v4-pro" => Some(ModelPricing {
            input_per_million: 1.89,
            output_per_million: 7.70,
        }),
        "glm-4-flash" => Some(ModelPricing {
            input_per_million: 0.10,
            output_per_million: 0.10,
        }),
        // GLM-5.1 pricing is currently a placeholder pending official
        // confirmation; the per-run `cost: $X` line will be approximate
        // until calibrated against the Zhipu billing dashboard.
        "glm-5.1" => Some(ModelPricing {
            input_per_million: 0.50,
            output_per_million: 2.00,
        }),
        _ => None,
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

/// Request for a structured JSON response conforming to a JSON schema.
pub struct StructuredRequest {
    pub messages: Vec<Message>,
    /// JSON Schema describing the expected response shape.
    pub schema: Value,
    /// Name for the schema (sent to the provider as `schema_name`).
    pub schema_name: String,
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

    /// Request a JSON response conforming to a caller-supplied schema.
    /// Default impl returns an error. Providers that support structured
    /// output (e.g. OpenAI-compatible) override this.
    async fn complete_structured(&self, _req: StructuredRequest) -> Result<Value> {
        Err(Error::Config(
            "provider does not support structured output".into(),
        ))
    }

    /// Stream a completion token-by-token.
    ///
    /// The default implementation delegates to [`complete`] and emits the
    /// entire content as a single delta via the channel (if configured).
    /// Providers that support native SSE streaming should override this.
    ///
    /// The `stream_tx` channel receives partial-token deltas as they are
    /// parsed. The returned `Completion` is the fully accumulated result.
    async fn stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        stream_tx: Option<StreamSender>,
    ) -> Result<Completion> {
        let completion = self.complete(messages, tools).await?;
        if let Some(tx) = stream_tx {
            if !completion.content.is_empty() {
                let _ = tx.send(completion.content.clone());
            }
        }
        Ok(completion)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_structured_returns_default_error() {
        // MockProvider overrides complete_structured. When no structured
        // responses are configured, it returns an error (triggering fallback).
        let provider = MockProvider::new(vec![]).with_structured_responses(vec![]);
        let req = StructuredRequest {
            messages: vec![Message::user("hi".to_string())],
            schema: serde_json::json!({"type": "object", "properties": {"answer": {"type": "string"}}}),
            schema_name: "test_schema".to_string(),
        };
        let result = provider.complete_structured(req).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("no structured responses configured"),
            "error should mention no structured responses: {msg}"
        );
    }

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
            cache_hit_tokens: 0,
            cache_miss_tokens: 0,
        };
        let u2 = TokenUsage {
            prompt_tokens: 20,
            completion_tokens: 30,
            total_tokens: 50,
            cache_hit_tokens: 0,
            cache_miss_tokens: 0,
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
            cache_hit_tokens: 0,
            cache_miss_tokens: 0,
        };
        let u2 = TokenUsage {
            prompt_tokens: 20,
            completion_tokens: 30,
            total_tokens: 50,
            cache_hit_tokens: 0,
            cache_miss_tokens: 0,
        };
        assert_eq!(u1.accumulate(u2), u2.accumulate(u1));
    }

    #[test]
    fn token_usage_accumulate_saturates() {
        let u1 = TokenUsage {
            prompt_tokens: u32::MAX,
            completion_tokens: 1,
            total_tokens: u32::MAX,
            cache_hit_tokens: 0,
            cache_miss_tokens: 0,
        };
        let u2 = TokenUsage {
            prompt_tokens: 1,
            completion_tokens: u32::MAX,
            total_tokens: u32::MAX,
            cache_hit_tokens: 0,
            cache_miss_tokens: 0,
        };
        let acc = u1.accumulate(u2);
        assert_eq!(acc.prompt_tokens, u32::MAX);
        assert_eq!(acc.completion_tokens, u32::MAX);
        assert_eq!(acc.total_tokens, u32::MAX);
    }

    #[test]
    fn cost_usd_handles_zero_usage() {
        let pricing = ModelPricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
        };
        let usage = TokenUsage::default();
        let cost = pricing.cost_usd(usage);
        assert!((cost - 0.0).abs() < 1e-9);
    }

    #[test]
    fn cost_usd_computes_simple_case() {
        let pricing = ModelPricing {
            input_per_million: 1.0,
            output_per_million: 1.0,
        };
        // 1M input tokens, 0 output
        let usage = TokenUsage {
            prompt_tokens: 1_000_000,
            completion_tokens: 0,
            total_tokens: 1_000_000,
            cache_hit_tokens: 0,
            cache_miss_tokens: 0,
        };
        let cost = pricing.cost_usd(usage);
        assert!((cost - 1.0).abs() < 1e-9);
    }

    #[test]
    fn cost_usd_mixes_input_and_output() {
        let pricing = ModelPricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
        };
        // 500K input + 250K output
        let usage = TokenUsage {
            prompt_tokens: 500_000,
            completion_tokens: 250_000,
            total_tokens: 750_000,
            cache_hit_tokens: 0,
            cache_miss_tokens: 0,
        };
        let cost = pricing.cost_usd(usage);
        // 0.5 * 1.0 + 0.25 * 2.0 = 0.5 + 0.5 = 1.0
        assert!((cost - 1.0).abs() < 1e-9);
    }

    #[test]
    fn pricing_for_known_models() {
        let p1 = pricing_for("MiniMax-M2");
        assert!(p1.is_some());
        assert!((p1.unwrap().input_per_million - 0.30).abs() < 1e-9);

        let p2 = pricing_for("deepseek-chat");
        assert!(p2.is_some());
        assert!((p2.unwrap().input_per_million - 0.27).abs() < 1e-9);
    }

    #[test]
    fn pricing_for_unknown_returns_none() {
        let p = pricing_for("unknown-model-xyz");
        assert!(p.is_none());
    }

    #[test]
    fn token_usage_accumulate_cache_fields() {
        let u1 = TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            cache_hit_tokens: 60,
            cache_miss_tokens: 40,
        };
        let u2 = TokenUsage {
            prompt_tokens: 200,
            completion_tokens: 100,
            total_tokens: 300,
            cache_hit_tokens: 120,
            cache_miss_tokens: 80,
        };
        let acc = u1.accumulate(u2);
        assert_eq!(acc.cache_hit_tokens, 180);
        assert_eq!(acc.cache_miss_tokens, 120);
        assert_eq!(acc.prompt_tokens, 300);
        assert_eq!(acc.completion_tokens, 150);
        assert_eq!(acc.total_tokens, 450);
    }
}
