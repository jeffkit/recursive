//! LLM pricing and retry policy.
//!
//! Contains the shared [`RetryPolicy`] struct used by all provider
//! implementations, the per-model [`ModelPricing`] struct, and lookup
//! helpers that consult the bundled `providers.toml` preset catalog.

use std::time::Duration;

use super::TokenUsage;

// ── Retry policy ────────────────────────────────────────────────────────────

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

// ── Model pricing ────────────────────────────────────────────────────────────

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
        let total_output_tokens = usage
            .completion_tokens
            .saturating_add(usage.reasoning_tokens);
        let out_cost = total_output_tokens as f64 * self.output_per_million / 1_000_000.0;
        in_cost + out_cost
    }
}

// ── Lookup helpers ────────────────────────────────────────────────────────────

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

/// Like [`context_window_tokens_for_model`] but consults the **effective**
/// catalog (remote cache + bundled + `providers.d/`) rather than the
/// compile-time `providers.toml` alone. Mirrors [`find_model_pricing_effective`]:
/// the TUI's `/model` picker lists models with their effective
/// `context_window`, so the input-box context gauge must read the same
/// source — otherwise it shows a stale bundled value for a model the user
/// has overridden to a different window in `providers.d/`.
pub fn context_window_tokens_for_model_effective(model: &str) -> usize {
    for preset in crate::providers::all_presets_effective() {
        for spec in &preset.models {
            if spec.name == model {
                // A 0 context_window in an override means "unspecified"
                // (e.g. an incomplete `providers.d` entry for a model the
                // upstream catalog hasn't filled in yet). Fall through to
                // the bundled lookup rather than reporting a nonsensical 0,
                // so the gauge never shows "ctx x/0 - 100%".
                if spec.context_window > 0 {
                    return spec.context_window;
                }
            }
        }
    }
    // No effective spec with a positive window — fall back to the bundled
    // catalog (which itself falls back to 128K for unknown models).
    context_window_tokens_for_model(model)
}

/// Compute the default compaction character-count threshold for a model.
///
/// Strategy (mirrors fake-cc `getAutoCompactThreshold`):
/// 1. Start from the model's context window in tokens.
/// 2. Reserve 20 000 tokens for the compaction summary output.
/// 3. Take 80 % of the remainder as the trigger point (leaves a comfortable
///    20 % buffer before the hard limit is hit).
/// 4. Convert tokens → characters using a conservative 4 chars/token ratio.
///
/// **Limitation**: the 4 chars/token ratio works well for English but
/// significantly underestimates token density in CJK languages (~1 char ≈
/// 2-3 tokens). Prefer [`default_compact_threshold_tokens`] when actual
/// prompt-token counts are available from the API response.
pub fn default_compact_threshold_chars(model: &str) -> usize {
    let context_tokens = context_window_tokens_for_model(model);
    let reserved_for_summary = 20_000_usize.min(context_tokens / 4);
    let effective_tokens = context_tokens.saturating_sub(reserved_for_summary);
    // 80 % of effective window, then 4 chars per token.
    (effective_tokens as f64 * 0.8 * 4.0) as usize
}

/// Compute the default compaction token threshold for a model.
///
/// Uses the same 80 % / reserve-for-summary strategy as
/// [`default_compact_threshold_chars`] but returns the raw token count
/// instead of converting to characters. This threshold is more reliable
/// than the character-based one for non-English content where the
/// 4-char/token assumption breaks down (e.g. CJK text uses ~1 char/token).
///
/// When actual `prompt_tokens` data is available from the API response,
/// comparing against this threshold is preferred over the char estimate.
pub fn default_compact_threshold_tokens(model: &str) -> u32 {
    let context_tokens = context_window_tokens_for_model(model);
    let reserved_for_summary = 20_000_usize.min(context_tokens / 4);
    let effective_tokens = context_tokens.saturating_sub(reserved_for_summary);
    // 80 % of effective window.
    (effective_tokens as f64 * 0.8) as u32
}

/// Returns pricing for a model by looking it up in the **effective** preset
/// catalog (remote cache + bundled + `providers.d/`), so per-token cost
/// reflects upstream catalog refreshes rather than just the compile-time
/// `providers.toml`. Returns `None` if the model is not listed or has no
/// pricing field.
pub fn pricing_for(model: &str) -> Option<ModelPricing> {
    let spec = crate::providers::find_model_pricing_effective(model)?;
    Some(ModelPricing {
        input_per_million: spec.input_per_million,
        output_per_million: spec.output_per_million,
        cache_hit_input_per_million: spec
            .cache_hit_input_per_million
            .unwrap_or(spec.input_per_million),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::TokenUsage;

    fn default_usage() -> TokenUsage {
        TokenUsage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            cache_hit_tokens: 0,
            cache_miss_tokens: 0,
            reasoning_tokens: 0,
        }
    }

    // ── RetryPolicy::backoff_for ─────────────────────────────────────────────

    #[test]
    fn retry_policy_returns_none_when_attempt_exceeds_max() {
        let policy = RetryPolicy::default(); // max_retries: 2
        assert!(
            policy.backoff_for(2, None, true).is_none(),
            "attempt == max_retries must return None"
        );
        assert!(
            policy.backoff_for(5, None, true).is_none(),
            "attempt > max_retries must return None"
        );
    }

    #[test]
    fn retry_policy_network_error_returns_some_backoff() {
        let policy = RetryPolicy::default();
        let result = policy.backoff_for(0, None, true);
        assert!(result.is_some(), "network error must trigger retry");
        assert_eq!(result.unwrap(), Duration::from_secs(1));
    }

    #[test]
    fn retry_policy_status_429_returns_some_backoff() {
        let policy = RetryPolicy::default();
        let result = policy.backoff_for(0, Some(429), false);
        assert!(result.is_some(), "429 must trigger retry");
    }

    #[test]
    fn retry_policy_status_503_returns_some_backoff() {
        let policy = RetryPolicy::default();
        let result = policy.backoff_for(0, Some(503), false);
        assert!(result.is_some(), "503 (5xx) must trigger retry");
    }

    #[test]
    fn retry_policy_status_200_returns_none() {
        let policy = RetryPolicy::default();
        let result = policy.backoff_for(0, Some(200), false);
        assert!(result.is_none(), "200 OK must not trigger retry");
    }

    #[test]
    fn retry_policy_status_400_returns_none() {
        let policy = RetryPolicy::default();
        let result = policy.backoff_for(0, Some(400), false);
        assert!(result.is_none(), "400 client error must not trigger retry");
    }

    #[test]
    fn retry_policy_backoff_doubles_per_attempt() {
        let policy = RetryPolicy {
            max_retries: 5,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(100),
        };
        assert_eq!(
            policy.backoff_for(0, None, true).unwrap(),
            Duration::from_secs(1)
        );
        assert_eq!(
            policy.backoff_for(1, None, true).unwrap(),
            Duration::from_secs(2)
        );
        assert_eq!(
            policy.backoff_for(2, None, true).unwrap(),
            Duration::from_secs(4)
        );
        assert_eq!(
            policy.backoff_for(3, None, true).unwrap(),
            Duration::from_secs(8)
        );
    }

    #[test]
    fn retry_policy_backoff_capped_at_max() {
        let policy = RetryPolicy {
            max_retries: 5,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(5),
        };
        let b3 = policy.backoff_for(3, None, true).unwrap(); // would be 8s without cap
        assert_eq!(
            b3,
            Duration::from_secs(5),
            "backoff must be capped at max_backoff"
        );
    }

    // ── ModelPricing::cost_usd ───────────────────────────────────────────────

    #[test]
    fn model_pricing_cost_no_tokens_is_zero() {
        let pricing = ModelPricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
            cache_hit_input_per_million: 0.5,
        };
        let cost = pricing.cost_usd(default_usage());
        assert_eq!(cost, 0.0);
    }

    #[test]
    fn model_pricing_cost_prompt_only() {
        let pricing = ModelPricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
            cache_hit_input_per_million: 0.5,
        };
        let usage = TokenUsage {
            prompt_tokens: 1_000_000,
            ..default_usage()
        };
        let cost = pricing.cost_usd(usage);
        assert!(
            (cost - 1.0).abs() < 1e-9,
            "1M input tokens at $1/M must cost $1, got {cost}"
        );
    }

    #[test]
    fn model_pricing_cost_completion_only() {
        let pricing = ModelPricing {
            input_per_million: 1.0,
            output_per_million: 3.0,
            cache_hit_input_per_million: 0.5,
        };
        let usage = TokenUsage {
            completion_tokens: 1_000_000,
            ..default_usage()
        };
        let cost = pricing.cost_usd(usage);
        assert!(
            (cost - 3.0).abs() < 1e-9,
            "1M completion tokens at $3/M must cost $3, got {cost}"
        );
    }

    #[test]
    fn model_pricing_cost_reasoning_tokens_add_to_output() {
        let pricing = ModelPricing {
            input_per_million: 0.0,
            output_per_million: 1.0,
            cache_hit_input_per_million: 0.0,
        };
        let usage = TokenUsage {
            completion_tokens: 500_000,
            reasoning_tokens: 500_000,
            ..default_usage()
        };
        let cost = pricing.cost_usd(usage);
        assert!(
            (cost - 1.0).abs() < 1e-9,
            "completion + reasoning = 1M output tokens at $1/M must cost $1, got {cost}"
        );
    }

    #[test]
    fn model_pricing_cost_cache_hit_applies_discounted_rate() {
        let pricing = ModelPricing {
            input_per_million: 1.0,
            output_per_million: 0.0,
            cache_hit_input_per_million: 0.1,
        };
        // 500K cache hits + 500K cache miss = $0.05 + $0.50 = $0.55
        let usage = TokenUsage {
            cache_hit_tokens: 500_000,
            cache_miss_tokens: 500_000,
            ..default_usage()
        };
        let cost = pricing.cost_usd(usage);
        let expected = 0.5 * 0.1 + 0.5 * 1.0; // 0.05 + 0.50
        assert!(
            (cost - expected).abs() < 1e-9,
            "cache-hit tokens use discounted rate, got {cost} vs {expected}"
        );
    }

    // ── context_window_tokens_for_model ─────────────────────────────────────

    #[test]
    fn context_window_unknown_model_returns_fallback() {
        let w = context_window_tokens_for_model("totally-unknown-model-xyz");
        assert_eq!(w, 128_000, "unknown models must fall back to 128K");
    }

    // ── context_window_tokens_for_model_effective ───────────────────────────

    #[test]
    fn context_window_effective_unknown_model_returns_fallback() {
        // The effective catalog falls back to the same 128K as the bundled
        // path for models it doesn't list. Asserted as an exact value (not
        // machine-dependent): no `providers.d` override can invent a spec
        // for an unknown model name.
        let w = context_window_tokens_for_model_effective("totally-unknown-model-xyz");
        assert_eq!(w, 128_000, "unknown models must fall back to 128K");
    }

    #[test]
    fn context_window_effective_known_model_is_nonzero() {
        // A catalogued model must resolve to a positive window. We avoid
        // asserting an exact value because the effective catalog honours
        // `providers.d` overrides + the remote cache, which differ per
        // machine — only the non-zero invariant is portable.
        let w = context_window_tokens_for_model_effective("gpt-4o");
        assert!(w > 0, "effective context window for gpt-4o must be > 0");
    }

    // ── default_compact_threshold_chars ─────────────────────────────────────

    #[test]
    fn compact_threshold_unknown_model_uses_fallback_128k() {
        let threshold = default_compact_threshold_chars("unknown-xyz");
        // 128_000 - min(20_000, 32_000)=20_000 → 108_000 * 0.8 * 4 = 345_600
        assert_eq!(threshold, 345_600);
    }

    #[test]
    fn compact_threshold_is_positive() {
        let t = default_compact_threshold_chars("gpt-4o");
        assert!(t > 0, "compact threshold must be positive for any model");
    }

    // ── default_compact_threshold_tokens ────────────────────────────────────

    #[test]
    fn compact_threshold_tokens_unknown_model_uses_fallback_128k() {
        let threshold = default_compact_threshold_tokens("unknown-xyz");
        // 128_000 - min(20_000, 32_000)=20_000 → 108_000 * 0.8 = 86_400
        assert_eq!(threshold, 86_400);
    }

    #[test]
    fn compact_threshold_tokens_glm5_2_uses_200k_window() {
        let threshold = default_compact_threshold_tokens("z-ai/glm-5.2");
        // 200_000 - min(20_000, 50_000)=20_000 → 180_000 * 0.8 = 144_000
        assert_eq!(threshold, 144_000);
    }

    #[test]
    fn compact_threshold_tokens_glm5_2_1m_uses_1m_window() {
        let threshold = default_compact_threshold_tokens("z-ai/glm-5.2[1m]");
        // 1_000_000 - min(20_000, 250_000)=20_000 → 980_000 * 0.8 = 784_000
        assert_eq!(threshold, 784_000);
    }

    #[test]
    fn compact_threshold_tokens_is_less_than_context_window() {
        for model in &["gpt-4o", "z-ai/glm-5.2", "deepseek-chat", "unknown-xyz"] {
            let window = context_window_tokens_for_model(model);
            let threshold = default_compact_threshold_tokens(model) as usize;
            assert!(
                threshold < window,
                "token threshold {threshold} must be below context window {window} for {model}"
            );
        }
    }
}
