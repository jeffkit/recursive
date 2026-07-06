//! LLM provider abstraction.
//!
//! A provider takes a transcript plus tool specs and returns either
//! free-form content, structured tool calls, or both. The [`ChatProvider`]
//! trait is the only thing the agent depends on; everything beyond it
//! (HTTP, retries, mocking) lives in adapters.
//!
//! ## Sub-modules
//!
//! - [`chat`] — data types: `Completion`, `ToolSpec`, `ToolCall`, `TokenUsage`, …
//! - [`pricing`] — `ModelPricing`, `RetryPolicy`, and catalog lookup helpers
//! - `openai` / `anthropic` / `mock` — concrete provider implementations

use async_trait::async_trait;
use serde_json::Value;

use crate::error::{Error, Result};
use crate::message::Message;

pub mod chat;
pub mod pricing;
pub mod search;

#[cfg(feature = "anthropic")]
pub mod anthropic;
pub mod mock;
pub mod openai;

// ── Re-exports: chat types ────────────────────────────────────────────────────

pub use chat::{
    Completion, StreamChunk, StreamSender, StructuredRequest, TokenUsage, ToolCall, ToolSpec,
};

// ── Re-exports: pricing ───────────────────────────────────────────────────────

pub use pricing::{
    context_window_tokens_for_model, default_compact_threshold_chars, pricing_for, ModelPricing,
    RetryPolicy,
};

// ── Re-exports: provider implementations ─────────────────────────────────────

#[cfg(feature = "anthropic")]
pub use anthropic::AnthropicProvider;
#[cfg(any(test, feature = "test-utils"))]
pub use mock::MockProvider;
pub use openai::OpenAiProvider;

// ── ChatProvider trait ────────────────────────────────────────────────────────

/// Core trait for LLM chat-completion providers.
///
/// Implementors must provide [`ChatProvider::complete`]. All other methods
/// have default implementations that delegate to `complete` or return
/// appropriate defaults, so adding a new provider only requires implementing
/// one `async fn`.
#[async_trait]
pub trait ChatProvider: Send + Sync {
    async fn complete(&self, messages: &[Message], tools: &[ToolSpec]) -> Result<Completion>;

    /// Whether this provider supports deferred tool loading via
    /// `tool_reference` content blocks (Anthropic API feature).
    ///
    /// When `true`, `run_core` will:
    /// - Strip deferred tools from the `tools` array sent to the LLM
    /// - Inject `<available-deferred-tools>` into the messages
    /// - Rely on `serialize_messages_anthropic` to convert ToolSearchTool
    ///   results into `tool_reference` blocks for schema expansion.
    ///
    /// When `false` (default), all tools are sent eagerly and
    /// `ToolSearchTool` is NOT registered in the registry.
    fn supports_deferred_tools(&self) -> bool {
        false
    }

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
    /// The default implementation delegates to [`ChatProvider::complete`] and emits the
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
            if let Some(reasoning) = &completion.reasoning_content {
                if !reasoning.is_empty() {
                    let _ = tx.send(StreamChunk::Reasoning(reasoning.clone()));
                }
            }
            if !completion.content.is_empty() {
                let _ = tx.send(StreamChunk::Text(completion.content.clone()));
            }
        }
        Ok(completion)
    }

    /// Search-aware streaming: model sees eager tools + `ToolSearchTool`;
    /// deferred tools become available only after the model requests them.
    ///
    /// The default implementation merges both lists and delegates to
    /// [`ChatProvider::stream`] — i.e., behaves like the legacy eager-only path.
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
    /// Wraps the prompt in a user [`Message`] and calls [`complete`](ChatProvider::complete)
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
            reasoning_tokens: 0,
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            cache_hit_tokens: 0,
            cache_miss_tokens: 0,
        };
        let u2 = TokenUsage {
            reasoning_tokens: 0,
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
            reasoning_tokens: 0,
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            cache_hit_tokens: 0,
            cache_miss_tokens: 0,
        };
        let u2 = TokenUsage {
            reasoning_tokens: 0,
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
            reasoning_tokens: 0,
            prompt_tokens: u32::MAX,
            completion_tokens: 1,
            total_tokens: u32::MAX,
            cache_hit_tokens: 0,
            cache_miss_tokens: 0,
        };
        let u2 = TokenUsage {
            reasoning_tokens: 0,
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
            cache_hit_input_per_million: 0.1,
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
        let usage = TokenUsage {
            reasoning_tokens: 0,
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
        let usage = TokenUsage {
            reasoning_tokens: 0,
            prompt_tokens: 500_000,
            completion_tokens: 250_000,
            total_tokens: 750_000,
            cache_hit_tokens: 0,
            cache_miss_tokens: 0,
        };
        let cost = pricing.cost_usd(usage);
        assert!((cost - 1.0).abs() < 1e-9);
    }

    #[test]
    fn pricing_for_known_models() {
        // Pin RECURSIVE_HOME so the effective catalog (remote cache +
        // bundled) collapses to bundled pricing — a stray
        // ~/.recursive/providers_cache.json on the developer's machine
        // must not change the asserted bundled prices.
        let _home = tempfile::tempdir().unwrap();
        let _pin = crate::test_util::PinnedRecursiveHome::new(_home.path());
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
            reasoning_tokens: 0,
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            cache_hit_tokens: 60,
            cache_miss_tokens: 40,
        };
        let u2 = TokenUsage {
            reasoning_tokens: 0,
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

    #[test]
    fn cost_usd_with_no_cache_hit_matches_old_behavior() {
        let pricing = ModelPricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
            cache_hit_input_per_million: 0.1,
        };
        let usage = TokenUsage {
            reasoning_tokens: 0,
            prompt_tokens: 1_000_000,
            completion_tokens: 500_000,
            total_tokens: 1_500_000,
            cache_hit_tokens: 0,
            cache_miss_tokens: 1_000_000,
        };
        let cost = pricing.cost_usd(usage);
        assert!((cost - 2.0).abs() < 1e-9);
    }

    #[test]
    fn cost_usd_with_cache_hit_applies_discount() {
        let pricing = ModelPricing {
            input_per_million: 0.27,
            output_per_million: 1.10,
            cache_hit_input_per_million: 0.027,
        };
        let usage = TokenUsage {
            reasoning_tokens: 0,
            prompt_tokens: 1_000,
            completion_tokens: 500,
            total_tokens: 1_500,
            cache_hit_tokens: 900,
            cache_miss_tokens: 100,
        };
        let cost = pricing.cost_usd(usage);
        let expected =
            900.0 * 0.027 / 1_000_000.0 + 100.0 * 0.27 / 1_000_000.0 + 500.0 * 1.10 / 1_000_000.0;
        assert!((cost - expected).abs() < 1e-9);
    }

    #[test]
    fn pricing_for_deepseek_has_cache_discount() {
        let _home = tempfile::tempdir().unwrap();
        let _pin = crate::test_util::PinnedRecursiveHome::new(_home.path());
        let pricing = pricing_for("deepseek-chat").expect("deepseek-chat should be known");
        assert!((pricing.input_per_million - 0.14).abs() < 1e-9);
        assert!((pricing.cache_hit_input_per_million - 0.0028).abs() < 1e-9);
    }

    #[test]
    fn pricing_for_unknown_model_returns_none() {
        let p = pricing_for("unknown-model-xyz");
        assert!(p.is_none());
    }

    #[test]
    fn token_usage_accumulate_preserves_cache_tokens() {
        let u1 = TokenUsage {
            reasoning_tokens: 0,
            prompt_tokens: 1000,
            completion_tokens: 100,
            total_tokens: 1100,
            cache_hit_tokens: 900,
            cache_miss_tokens: 100,
        };
        let u2 = TokenUsage {
            reasoning_tokens: 0,
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

    #[test]
    fn token_usage_accumulate_sums_reasoning() {
        let a = TokenUsage {
            reasoning_tokens: 100,
            ..Default::default()
        };
        let b = TokenUsage {
            reasoning_tokens: 250,
            ..Default::default()
        };
        let c = a.accumulate(b);
        assert_eq!(c.reasoning_tokens, 350);
    }

    #[test]
    fn context_window_known_models() {
        assert_eq!(
            context_window_tokens_for_model("claude-sonnet-4-6"),
            1_000_000
        );
        assert_eq!(
            context_window_tokens_for_model("claude-opus-4-7"),
            1_000_000
        );
        assert_eq!(context_window_tokens_for_model("MiniMax-M3"), 1_048_576);
        assert_eq!(context_window_tokens_for_model("deepseek-chat"), 1_000_000);
        assert_eq!(
            context_window_tokens_for_model("deepseek-reasoner"),
            1_000_000
        );
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
        assert_eq!(
            context_window_tokens_for_model("some-future-model"),
            128_000
        );
    }

    #[test]
    fn default_compact_threshold_is_reasonable() {
        let ds = default_compact_threshold_chars("deepseek-chat");
        assert!(ds > 500_000, "deepseek threshold too small: {ds}");
        assert!(
            ds < 4_000_000,
            "deepseek threshold suspiciously large: {ds}"
        );

        let cl = default_compact_threshold_chars("claude-sonnet-4-6");
        assert!(cl > 500_000, "claude threshold too small: {cl}");
        assert!(cl < 4_000_000, "claude threshold suspiciously large: {cl}");

        let unk = default_compact_threshold_chars("unknown-model");
        assert!(unk > 0);
    }

    // ── ChatProvider default method tests ─────────────────────────────────────

    /// A minimal provider that only overrides `complete()`.
    /// All other ChatProvider methods use their trait defaults.
    struct MinimalProvider {
        response: String,
    }

    #[async_trait::async_trait]
    impl ChatProvider for MinimalProvider {
        async fn complete(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
        ) -> Result<Completion> {
            Ok(Completion {
                content: self.response.clone(),
                tool_calls: vec![],
                reasoning_content: None,
                usage: Some(TokenUsage::default()),
                finish_reason: None,
            })
        }
    }

    #[test]
    fn default_supports_deferred_tools_is_false() {
        // kills `replace ChatProvider::supports_deferred_tools -> bool with true`
        let provider = MinimalProvider { response: "hi".into() };
        assert!(
            !provider.supports_deferred_tools(),
            "default supports_deferred_tools must be false"
        );
    }

    #[tokio::test]
    async fn default_stream_sends_non_empty_content_to_channel() {
        // kills `delete ! in ChatProvider::stream` at line 132
        // With the mutant, content is only sent when IS empty — so this test
        // would receive no chunk and the assertion would fail.
        use tokio::sync::mpsc;
        let provider = MinimalProvider { response: "hello from stream".into() };
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamChunk>();

        let completion = provider.stream(&[], &[], Some(tx)).await.unwrap();
        assert_eq!(completion.content, "hello from stream");

        // The channel should have received a Text chunk with the content.
        let chunk = rx.try_recv().expect("stream must emit at least one chunk");
        match chunk {
            StreamChunk::Text(t) => assert_eq!(t, "hello from stream"),
            other => panic!("expected Text chunk, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn default_stream_does_not_send_chunk_for_empty_content() {
        // Verifies the positive case: empty content → no Text chunk sent
        use tokio::sync::mpsc;
        let provider = MinimalProvider { response: String::new() };
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamChunk>();

        let completion = provider.stream(&[], &[], Some(tx)).await.unwrap();
        assert!(completion.content.is_empty());

        // No Text chunk should have been sent for empty content.
        assert!(
            rx.try_recv().is_err(),
            "empty content must not emit a Text chunk"
        );
    }
}
