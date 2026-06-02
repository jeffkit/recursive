//! LLM provider abstraction.
//!
//! A provider takes a transcript plus tool specs and returns either
//! free-form content, structured tool calls, or both. The trait is the
//! only thing the agent depends on; everything beyond it (HTTP, retries,
//! mocking) lives in adapters.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

use crate::error::Error;
use crate::error::Result;
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

/// Returns pricing for known models, or None if unknown.
pub fn pricing_for(model: &str) -> Option<ModelPricing> {
    match model {
        "MiniMax-M2" => Some(ModelPricing {
            input_per_million: 0.30,
            output_per_million: 1.20,
            // No known cache discount; use full rate (conservative)
            cache_hit_input_per_million: 0.30,
        }),
        "deepseek-chat" | "deepseek-v4-flash" => Some(ModelPricing {
            input_per_million: 0.27,
            output_per_million: 1.10,
            // DeepSeek: cache hit = 10% of input rate
            cache_hit_input_per_million: 0.027,
        }),
        // V4-Pro is ~7× flash on input; placeholder until calibrated
        // against the DeepSeek billing dashboard.
        "deepseek-v4-pro" => Some(ModelPricing {
            input_per_million: 1.89,
            output_per_million: 7.70,
            // DeepSeek: cache hit = 10% of input rate
            cache_hit_input_per_million: 0.189,
        }),
        "glm-4-flash" => Some(ModelPricing {
            input_per_million: 0.10,
            output_per_million: 0.10,
            // No known cache discount; use full rate
            cache_hit_input_per_million: 0.10,
        }),
        // GLM-5.1 pricing is currently a placeholder pending official
        // confirmation; the per-run `cost: $X` line will be approximate
        // until calibrated against the Zhipu billing dashboard.
        "glm-5.1" => Some(ModelPricing {
            input_per_million: 0.50,
            output_per_million: 2.00,
            // No known cache discount; use full rate
            cache_hit_input_per_million: 0.50,
        }),
        _ => {
            // Unknown model: use conservative default (full rate for cache hits,
            // i.e., no discount). This matches the goal spec: "use full rate
            // (conservative)".
            None
        }
    }
}

/// Load pricing from a YAML file.
/// The file should have a "models" key mapping model names to pricing structs.
/// Returns a HashMap of model name -> ModelPricing.
pub fn load_pricing_from_yaml(path: &Path) -> Result<HashMap<String, ModelPricing>> {
    use std::fs;
    use std::io::{self, BufRead};

    let file = fs::File::open(path).map_err(Error::Io)?;
    let reader = io::BufReader::new(file);

    let mut models: HashMap<String, ModelPricing> = HashMap::new();
    let mut current_model: Option<String> = None;
    let mut current_pricing: Option<ModelPricingBuilder> = None;
    let mut in_models_section = false;

    // Simple YAML parser for the flat structure we expect.
    // Lines are either:
    //   models:
    //   <model_name>:
    //     key: value
    for line in reader.lines() {
        let line = line.map_err(Error::Io)?;

        // Skip empty lines and comments
        if line.trim().is_empty() || line.trim().starts_with('#') {
            continue;
        }

        // Count leading spaces to determine indentation level
        let leading_spaces = line.len() - line.trim_start().len();
        let trimmed = line.trim();

        // Top-level "models:" key
        if trimmed == "models:" {
            in_models_section = true;
            continue;
        }

        // If we're in the models section
        if in_models_section {
            // Model name line: 2 spaces indent, ends with colon
            // e.g., "  deepseek-chat:" -> model name is "deepseek-chat"
            if leading_spaces == 2 && trimmed.ends_with(':') {
                // Save previous model if any
                if let (Some(name), Some(builder)) = (current_model.take(), current_pricing.take())
                {
                    if let Some(pricing) = builder.build() {
                        models.insert(name, pricing);
                    }
                }
                // Extract model name (remove trailing colon)
                let model_name = trimmed.trim_end_matches(':').to_string();
                current_model = Some(model_name);
                current_pricing = Some(ModelPricingBuilder::default());
                continue;
            }

            // Field line: 4+ spaces indent, contains "key: value"
            // e.g., "    input_per_million: 0.27"
            if leading_spaces >= 4 && current_model.is_some() {
                if let Some(ref mut builder) = current_pricing {
                    if let Some((key, value)) = trimmed.split_once(':') {
                        let key = key.trim();
                        // Strip inline comments: "0.027  # 10% of input" → "0.027"
                        let value = value.split('#').next().unwrap_or(value).trim();
                        if let Err(e) = builder.parse_field(key, value) {
                            return Err(Error::Config {
                                message: format!("error parsing {}: {}", path.display(), e),
                            });
                        }
                    }
                }
            }
        }
    }

    // Save last model
    if let (Some(name), Some(builder)) = (current_model, current_pricing) {
        if let Some(pricing) = builder.build() {
            models.insert(name, pricing);
        }
    }

    Ok(models)
}

/// Helper to build ModelPricing from YAML fields.
#[derive(Default)]
struct ModelPricingBuilder {
    input_per_million: Option<f64>,
    output_per_million: Option<f64>,
    cache_hit_input_per_million: Option<f64>,
}

impl ModelPricingBuilder {
    fn parse_field(&mut self, key: &str, value: &str) -> Result<(), String> {
        let value: f64 = value
            .parse()
            .map_err(|_| format!("invalid float: {}", value))?;
        match key {
            "input_per_million" => self.input_per_million = Some(value),
            "output_per_million" => self.output_per_million = Some(value),
            "cache_hit_input_per_million" => self.cache_hit_input_per_million = Some(value),
            _ => return Err(format!("unknown field: {}", key)),
        }
        Ok(())
    }

    fn build(self) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: self.input_per_million?,
            output_per_million: self.output_per_million?,
            cache_hit_input_per_million: self
                .cache_hit_input_per_million
                .unwrap_or(self.input_per_million.unwrap_or(0.0)),
        })
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
        // Input: $0.27/M, cache hit should be 10% = $0.027/M
        assert!((pricing.input_per_million - 0.27).abs() < 1e-9);
        assert!((pricing.cache_hit_input_per_million - 0.027).abs() < 1e-9);
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

    /// Test loading pricing from YAML file.
    #[test]
    fn load_pricing_from_yaml_parses_file() {
        let temp_dir = std::env::temp_dir();
        let yaml_path = temp_dir.join("test_pricing.yaml");
        std::fs::write(
            &yaml_path,
            r#"
models:
  test-model:
    input_per_million: 1.0
    output_per_million: 2.0
    cache_hit_input_per_million: 0.1
"#,
        )
        .unwrap();

        let result = load_pricing_from_yaml(&yaml_path);
        assert!(result.is_ok(), "load should succeed: {:?}", result.err());
        let pricing = result.unwrap();
        assert!(
            pricing.contains_key("test-model"),
            "should contain test-model: {:?}",
            pricing.keys().collect::<Vec<_>>()
        );
        let p = pricing.get("test-model").unwrap();
        assert!((p.input_per_million - 1.0).abs() < 1e-9);
        assert!((p.output_per_million - 2.0).abs() < 1e-9);
        assert!((p.cache_hit_input_per_million - 0.1).abs() < 1e-9);

        std::fs::remove_file(&yaml_path).ok();
    }

    /// Test external pricing overrides hardcoded values.
    #[test]
    fn load_pricing_from_yaml_overrides_hardcoded() {
        let temp_dir = std::env::temp_dir();
        let yaml_path = temp_dir.join("test_pricing_override.yaml");
        // Override deepseek-chat with a different rate
        std::fs::write(
            &yaml_path,
            r#"
models:
  deepseek-chat:
    input_per_million: 99.0
    output_per_million: 99.0
    cache_hit_input_per_million: 9.9
"#,
        )
        .unwrap();

        let result = load_pricing_from_yaml(&yaml_path);
        assert!(result.is_ok());
        let pricing = result.unwrap();
        let p = pricing
            .get("deepseek-chat")
            .expect("should have deepseek-chat");
        // Verify the override is loaded
        assert!((p.input_per_million - 99.0).abs() < 1e-9);

        // Hardcoded should still work for other models
        assert!(pricing.contains_key("deepseek-chat"));
        // MiniMax-M2 is not in the external file, so it shouldn't be here
        assert!(!pricing.contains_key("MiniMax-M2"));

        std::fs::remove_file(&yaml_path).ok();
    }

    /// Test missing model falls back to hardcoded.
    #[test]
    fn load_pricing_from_yaml_missing_model_falls_back() {
        // When loading external pricing, models not in the external file
        // should return None from pricing_for, which falls back to hardcoded.
        // This is tested by verifying pricing_for still returns hardcoded
        // values for models not in the external file.
        let temp_dir = std::env::temp_dir();
        let yaml_path = temp_dir.join("test_pricing_partial.yaml");
        // Only include one model
        std::fs::write(
            &yaml_path,
            r#"
models:
  test-only:
    input_per_million: 1.0
    output_per_million: 1.0
    cache_hit_input_per_million: 0.1
"#,
        )
        .unwrap();

        let result = load_pricing_from_yaml(&yaml_path);
        assert!(result.is_ok());
        let external = result.unwrap();

        // External has only test-only
        assert!(
            external.contains_key("test-only"),
            "should have test-only: {:?}",
            external.keys().collect::<Vec<_>>()
        );
        // MiniMax-M2 is NOT in external, so pricing_for should return hardcoded
        let fallback = pricing_for("MiniMax-M2");
        assert!(fallback.is_some());
        assert!((fallback.unwrap().input_per_million - 0.30).abs() < 1e-9);

        std::fs::remove_file(&yaml_path).ok();
    }

    /// Test malformed YAML returns descriptive error.
    #[test]
    fn load_pricing_from_yaml_malformed_error() {
        let temp_dir = std::env::temp_dir();
        let yaml_path = temp_dir.join("test_pricing_malformed.yaml");
        // Invalid YAML (invalid float value)
        std::fs::write(
            &yaml_path,
            r#"
models:
  bad-model:
    input_per_million: not_a_number
"#,
        )
        .unwrap();

        let result = load_pricing_from_yaml(&yaml_path);
        assert!(result.is_err(), "should fail with bad data");
        let err = result.unwrap_err();
        // Should contain some error message
        let err_str = err.to_string();
        assert!(
            err_str.contains("error parsing") || err_str.contains("invalid float"),
            "error should mention parsing issue: {}",
            err_str
        );

        std::fs::remove_file(&yaml_path).ok();
    }
}
