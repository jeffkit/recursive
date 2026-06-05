//! LLM provider abstraction.
//!
//! A provider takes a transcript plus tool specs and returns either
//! free-form content, structured tool calls, or both. The trait is the
//! only thing the agent depends on; everything beyond it (HTTP, retries,
//! mocking) lives in adapters.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::message::Message;

use tokio::sync::mpsc;

#[cfg(feature = "anthropic")]
pub mod anthropic;
pub mod mock;
pub mod openai;
pub mod search;

#[cfg(feature = "anthropic")]
pub use anthropic::AnthropicProvider;
#[cfg(any(test, feature = "test-utils"))]
pub use mock::MockProvider;
pub use openai::OpenAiProvider;

/// Channel sender for streaming partial tokens during a streaming LLM call.
/// Each `String` is a delta chunk (partial token) emitted by the provider.
pub type StreamSender = mpsc::UnboundedSender<String>;

// ── Shared retry policy ────────────────────────────────────────────────────

/// Retry policy for transient LLM provider failures (network timeouts, 5xx).
///
/// Shared across all provider implementations to keep retry semantics
/// consistent. Each provider stores one instance in its struct and calls
/// [`RetryPolicy::backoff_for`] after every failed HTTP attempt.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_retries: usize,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(8),
        }
    }
}

impl RetryPolicy {
    /// Returns `Some(backoff)` if the caller should sleep-and-retry, or `None`
    /// to propagate the error. `attempt` is 0-indexed (0 = after the first failure).
    pub fn backoff_for(
        &self,
        attempt: usize,
        status: Option<u16>,
        is_network_error: bool,
    ) -> Option<Duration> {
        if attempt >= self.max_retries {
            return None;
        }
        let is_transient =
            is_network_error || status.is_some_and(|s| s == 429 || (500..600).contains(&s));
        if !is_transient {
            return None;
        }
        let backoff = self.initial_backoff * 2u32.saturating_pow(attempt as u32);
        Some(backoff.min(self.max_backoff))
    }
}

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
    /// Price per million tokens for cache-hit prompts.
    /// Defaults to 10% of input rate (DeepSeek's known discount).
    pub cache_hit_input_per_million: f64,
}

impl ModelPricing {
    /// USD cost for the given usage at this pricing.
    pub fn cost_usd(&self, usage: TokenUsage) -> f64 {
        let in_cost = if usage.cache_hit_tokens > 0 {
            // Apply cache-hit discount: use cache_hit_input_per_million for cached tokens
            let cache_hit =
                usage.cache_hit_tokens as f64 * self.cache_hit_input_per_million / 1_000_000.0;
            // Use full rate for cache-miss tokens (which is prompt - cache_hit)
            // Note: cache_miss_tokens may not equal prompt - cache_hit due to rounding,
            // but the difference is negligible for billing purposes
            let cache_miss = usage.cache_miss_tokens as f64 * self.input_per_million / 1_000_000.0;
            cache_hit + cache_miss
        } else {
            (usage.prompt_tokens as f64) * self.input_per_million / 1_000_000.0
        };
        let out_cost = (usage.completion_tokens as f64) * self.output_per_million / 1_000_000.0;
        in_cost + out_cost
    }
}

/// Returns the context window size in tokens for the given model.
///
/// The value is looked up from the bundled `providers.toml` preset catalog.
/// Unknown models (not listed in any preset) fall back to a conservative
/// 128 K token default — the minimum window common to all current-generation
/// frontier models.
pub fn context_window_tokens_for_model(model: &str) -> usize {
    use crate::providers::all_presets;
    for preset in all_presets() {
        for spec in &preset.models {
            if spec.name == model {
                return spec.context_window;
            }
        }
    }
    // Conservative fallback for models not listed in providers.toml.
    128_000
}

/// Compute the default compaction character-count threshold for a model.
///
/// Strategy (mirrors fake-cc `getAutoCompactThreshold`):
/// 1. Start from the model's context window in tokens.
/// 2. Reserve 20 000 tokens for the compaction summary output.
/// 3. Take 80 % of the remainder as the trigger point (leaves a comfortable
///    20 % buffer before the hard limit is hit).
/// 4. Convert tokens → characters using a conservative 4 chars/token ratio.
pub fn default_compact_threshold_chars(model: &str) -> usize {
    let context_tokens = context_window_tokens_for_model(model);
    let reserved_for_summary = 20_000_usize.min(context_tokens / 4);
    let effective_tokens = context_tokens.saturating_sub(reserved_for_summary);
    // 80 % of effective window, then 4 chars per token.
    (effective_tokens as f64 * 0.8 * 4.0) as usize
}

/// Returns pricing for a model by looking it up in the bundled `providers.toml`.
/// Returns `None` if the model is not listed or has no pricing field.
pub fn pricing_for(model: &str) -> Option<ModelPricing> {
    let spec = crate::providers::find_model_pricing(model)?;
    Some(ModelPricing {
        input_per_million: spec.input_per_million,
        output_per_million: spec.output_per_million,
        cache_hit_input_per_million: spec
            .cache_hit_input_per_million
            .unwrap_or(spec.input_per_million),
    })
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

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn complete(&self, messages: &[Message], tools: &[ToolSpec]) -> Result<Completion>;

    /// Variant that accepts a partition between eager and deferred
    /// tools. Providers that support native deferred loading (e.g.
    /// Anthropic via `defer_loading: true` + `tool_reference` content
    /// blocks) override this. The default implementation concatenates
    /// the two lists (dropping the hints) and calls `complete()` —
    /// i.e., it ignores the partition and behaves identically to the
    /// legacy interface.
    ///
    /// Providers that do NOT support deferred tool loading (e.g. the
    /// OpenAI provider) inherit this default and see every tool as
    /// eager. No code change is required in those providers.
    async fn complete_with_search(
        &self,
        messages: &[Message],
        eager_tools: &[(ToolSpec, Option<String>)],
        deferred_tools: &[(ToolSpec, Option<String>)],
    ) -> Result<Completion> {
        let all: Vec<ToolSpec> = eager_tools
            .iter()
            .chain(deferred_tools.iter())
            .map(|(spec, _)| spec.clone())
            .collect();
        self.complete(messages, &all).await
    }

    /// Request a JSON response conforming to a caller-supplied schema.
    /// Default impl returns an error. Providers that support structured
    /// output (e.g. OpenAI-compatible) override this.
    async fn complete_structured(&self, _req: StructuredRequest) -> Result<Value> {
        Err(Error::Config {
            message: "provider does not support structured output".into(),
        })
    }

    /// Stream a completion token-by-token.
    ///
    /// The default implementation delegates to [`LlmProvider::complete`] and emits the
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

    /// Search-aware streaming: model sees eager tools + `ToolSearchTool`;
    /// deferred tools become available only after the model requests them.
    ///
    /// The default implementation merges both lists and delegates to
    /// [`LlmProvider::stream`] — i.e., behaves like the legacy eager-only path.
    /// Providers that support deferred tool loading (e.g. Anthropic) override
    /// this to inject `ToolSearchTool` into the eager set and handle the
    /// search loop across multiple streaming rounds.
    async fn stream_with_search(
        &self,
        messages: &[Message],
        eager_tools: &[(ToolSpec, Option<String>)],
        deferred_tools: &[(ToolSpec, Option<String>)],
        stream_tx: Option<StreamSender>,
    ) -> Result<Completion> {
        let all: Vec<ToolSpec> = eager_tools
            .iter()
            .chain(deferred_tools.iter())
            .map(|(spec, _)| spec.clone())
            .collect();
        self.stream(messages, &all, stream_tx).await
    }

    /// Simple completion with a single user prompt.
    ///
    /// Wraps the prompt in a user [`Message`] and calls [`complete`](LlmProvider::complete)
    /// with no tools. Providers that support temperature or other controls
    /// should override this method. The default implementation ignores
    /// `temperature`.
    async fn complete_simple(&self, prompt: &str, _temperature: f32) -> Result<String> {
        let messages = vec![Message::user(prompt.to_string())];
        let completion = self.complete(&messages, &[]).await?;
        Ok(completion.content)
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
            cache_hit_input_per_million: 0.1, // 10% discount
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
            cache_hit_input_per_million: 0.1,
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
            cache_hit_input_per_million: 0.1,
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
        let p1 = pricing_for("MiniMax-M3");
        assert!(p1.is_some());
        assert!((p1.unwrap().input_per_million - 0.30).abs() < 1e-9);

        let p2 = pricing_for("deepseek-chat");
        assert!(p2.is_some());
        assert!((p2.unwrap().input_per_million - 0.14).abs() < 1e-9);
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

    /// Backward compat: cache_hit_tokens = 0 should return same as before.
    #[test]
    fn cost_usd_with_no_cache_hit_matches_old_behavior() {
        let pricing = ModelPricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
            cache_hit_input_per_million: 0.1, // 10% discount
        };
        // No cache hits
        let usage = TokenUsage {
            prompt_tokens: 1_000_000,
            completion_tokens: 500_000,
            total_tokens: 1_500_000,
            cache_hit_tokens: 0,
            cache_miss_tokens: 1_000_000,
        };
        // Old calculation: 1M * $1/M + 500K * $2/M = $1.00 + $1.00 = $2.00
        let cost = pricing.cost_usd(usage);
        assert!((cost - 2.0).abs() < 1e-9);
    }

    /// Cache hit tokens get discounted rate (DeepSeek 10% of input rate).
    #[test]
    fn cost_usd_with_cache_hit_applies_discount() {
        // DeepSeek pricing: $0.27/M input, $0.027/M for cache hits
        let pricing = ModelPricing {
            input_per_million: 0.27,
            output_per_million: 1.10,
            cache_hit_input_per_million: 0.027,
        };
        // 900 cache hit + 100 cache miss = 1000 prompt tokens
        let usage = TokenUsage {
            prompt_tokens: 1_000,
            completion_tokens: 500,
            total_tokens: 1_500,
            cache_hit_tokens: 900,
            cache_miss_tokens: 100,
        };
        let cost = pricing.cost_usd(usage);
        // Cache hit: 900 * 0.027/1M = 0.0000243
        // Cache miss: 100 * 0.27/1M = 0.000027
        // Output: 500 * 1.10/1M = 0.00055
        // Total: 0.0000243 + 0.000027 + 0.00055 = 0.0006013
        let expected =
            900.0 * 0.027 / 1_000_000.0 + 100.0 * 0.27 / 1_000_000.0 + 500.0 * 1.10 / 1_000_000.0;
        assert!((cost - expected).abs() < 1e-9);
    }

    /// Verify known model has correct cache-hit pricing.
    #[test]
    fn pricing_for_deepseek_has_cache_discount() {
        let pricing = pricing_for("deepseek-chat").expect("deepseek-chat should be known");
        // deepseek-chat now routes to deepseek-v4-flash: $0.14/M input, $0.0028/M cache hit
        assert!((pricing.input_per_million - 0.14).abs() < 1e-9);
        assert!((pricing.cache_hit_input_per_million - 0.0028).abs() < 1e-9);
    }

    /// Unknown model returns None (cost won't be printed - conservative).
    #[test]
    fn pricing_for_unknown_model_returns_none() {
        let p = pricing_for("unknown-model-xyz");
        assert!(p.is_none());
    }

    /// Verify accumulated TokenUsage preserves cache_hit_tokens sum.
    #[test]
    fn token_usage_accumulate_preserves_cache_tokens() {
        let u1 = TokenUsage {
            prompt_tokens: 1000,
            completion_tokens: 100,
            total_tokens: 1100,
            cache_hit_tokens: 900,
            cache_miss_tokens: 100,
        };
        let u2 = TokenUsage {
            prompt_tokens: 2000,
            completion_tokens: 200,
            total_tokens: 2200,
            cache_hit_tokens: 1800,
            cache_miss_tokens: 200,
        };
        let acc = u1.accumulate(u2);
        assert_eq!(acc.cache_hit_tokens, 2700);
        assert_eq!(acc.cache_miss_tokens, 300);
        assert_eq!(acc.prompt_tokens, 3000);
    }

    // ── context_window_tokens_for_model / default_compact_threshold_chars ─────

    #[test]
    fn context_window_known_models() {
        // Names must exactly match providers.toml entries.
        assert_eq!(
            context_window_tokens_for_model("claude-sonnet-4-6"),
            1_000_000
        );
        assert_eq!(context_window_tokens_for_model("claude-opus-4-7"), 1_000_000);
        assert_eq!(context_window_tokens_for_model("MiniMax-M3"), 1_048_576);
        assert_eq!(context_window_tokens_for_model("deepseek-chat"), 1_000_000);
        assert_eq!(context_window_tokens_for_model("deepseek-reasoner"), 1_000_000);
        assert_eq!(context_window_tokens_for_model("gpt-4o"), 128_000);
        assert_eq!(context_window_tokens_for_model("gpt-4o-mini"), 128_000);
        assert_eq!(context_window_tokens_for_model("moonshot-v1-8k"), 8_000);
        assert_eq!(
            context_window_tokens_for_model("doubao-1-5-pro-256k"),
            256_000
        );
        assert_eq!(context_window_tokens_for_model("gemini-2.5-pro"), 1_048_576);
    }

    #[test]
    fn context_window_unknown_model_fallback() {
        // A model not listed in providers.toml → conservative 128 K default.
        assert_eq!(
            context_window_tokens_for_model("some-future-model"),
            128_000
        );
    }

    #[test]
    fn default_compact_threshold_is_reasonable() {
        // deepseek-chat: 1M tokens → threshold should be large
        let ds = default_compact_threshold_chars("deepseek-chat");
        assert!(ds > 500_000, "deepseek threshold too small: {ds}");
        assert!(ds < 4_000_000, "deepseek threshold suspiciously large: {ds}");

        // claude-sonnet-4-6: 1M tokens → threshold should be large
        let cl = default_compact_threshold_chars("claude-sonnet-4-6");
        assert!(cl > 500_000, "claude threshold too small: {cl}");
        assert!(cl < 4_000_000, "claude threshold suspiciously large: {cl}");

        // unknown model: threshold must be positive (falls back to 128K window)
        let unk = default_compact_threshold_chars("unknown-model");
        assert!(unk > 0);
    }
}
