//! Token usage accounting and cost estimation for the Recursive TUI.
//!
//! Contains [`UsageStats`] for per-session token accumulation, [`TurnState`]
//! for in-flight turn tracking, and helpers to detect the active model and
//! estimate dollar cost from token counts.

use std::time::Instant;

// ──────────────────────────────────────────────────────────────────────
// Usage / turn telemetry
// ──────────────────────────────────────────────────────────────────────

/// Token usage and timing accumulated across the session.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageStats {
    /// Most recent per-turn input tokens.
    pub input_tokens: u64,
    /// Most recent per-turn output tokens.
    pub output_tokens: u64,
    /// Cumulative input tokens across all turns.
    pub total_input: u64,
    /// Cumulative output tokens across all turns.
    pub total_output: u64,
    /// Most recent per-turn cache-hit tokens.
    pub cache_hit_tokens: u64,
    /// Most recent per-turn cache-miss tokens.
    pub cache_miss_tokens: u64,
    /// Cumulative cache-hit tokens across all turns.
    pub total_cache_hit: u64,
    /// Cumulative cache-miss tokens across all turns.
    pub total_cache_miss: u64,
    /// Cache-hit tokens summed over the in-progress / most recent turn.
    ///
    /// Reset to zero at the start of each turn via [`UsageStats::begin_turn`].
    /// Used to compute [`UsageStats::last_turn_cache_pct`] when the turn ends.
    pub turn_cache_hit: u64,
    /// Cache-miss tokens summed over the in-progress / most recent turn.
    pub turn_cache_miss: u64,
    /// Cache-hit rate (0.0–100.0) of the most recently *completed* turn.
    ///
    /// `None` until the first turn finishes. The status bar reads this
    /// instead of computing from `turn_cache_hit` / `turn_cache_miss`
    /// live, so the figure stays stable across the active turn and only
    /// refreshes when [`UsageStats::snapshot_turn_cache_pct`] runs at
    /// `TurnFinished`. This trades intra-turn visibility for a non-
    /// flickering number, which matters on long turns with many LLM
    /// responses (tool-use loops, retries) where the live rate can hop
    /// on every step.
    pub last_turn_cache_pct: Option<f64>,
    /// Most recent LLM round-trip latency, in milliseconds.
    pub last_latency_ms: u64,
    /// Total prompt size of the most recent LLM call — the best proxy
    /// for "how much of the context window is in use right now". Computed
    /// as `max(input_tokens, cache_hit + cache_miss)` so it is correct
    /// both for providers that report cache breakdowns (Anthropic, where
    /// `input_tokens` excludes cached tokens) and for providers that
    /// don't (OpenAI without cache details, where the cache fields are 0
    /// and `input_tokens` already carries the full prompt size). Shown as
    /// a live usage gauge at the bottom-right of the input box.
    pub last_prompt_tokens: u64,
}

impl UsageStats {
    /// Fold a `Usage` event into the stats. Treats incoming numbers as
    /// per-turn deltas and accumulates them into the running totals.
    pub fn record(&mut self, input_tokens: u64, output_tokens: u64) {
        self.input_tokens = input_tokens;
        self.output_tokens = output_tokens;
        self.total_input = self.total_input.saturating_add(input_tokens);
        self.total_output = self.total_output.saturating_add(output_tokens);
    }

    /// Fold a `Usage` event including cache fields into the stats.
    /// Treats incoming numbers as per-turn deltas and accumulates them
    /// into the running totals.
    pub fn record_with_cache(
        &mut self,
        input_tokens: u64,
        output_tokens: u64,
        cache_hit_tokens: u64,
        cache_miss_tokens: u64,
    ) {
        self.record(input_tokens, output_tokens);
        self.cache_hit_tokens = cache_hit_tokens;
        self.cache_miss_tokens = cache_miss_tokens;
        self.total_cache_hit = self.total_cache_hit.saturating_add(cache_hit_tokens);
        self.total_cache_miss = self.total_cache_miss.saturating_add(cache_miss_tokens);
        self.turn_cache_hit = self.turn_cache_hit.saturating_add(cache_hit_tokens);
        self.turn_cache_miss = self.turn_cache_miss.saturating_add(cache_miss_tokens);
        // Total prompt size of this LLM call. For Anthropic `input_tokens`
        // excludes cached tokens, so the cache sum is the real prompt size;
        // for OpenAI without cache reporting the cache sum is 0 and
        // `input_tokens` already carries the full prompt. Take the max.
        let cache_sum = cache_hit_tokens.saturating_add(cache_miss_tokens);
        self.last_prompt_tokens = input_tokens.max(cache_sum);
    }

    /// Reset the per-turn cache counters. Called when a new turn starts so the
    /// status bar reports the cache-hit rate for the current turn rather than
    /// the whole session.
    pub fn begin_turn(&mut self) {
        self.turn_cache_hit = 0;
        self.turn_cache_miss = 0;
    }

    /// Snapshot the current per-turn cache counters into
    /// [`UsageStats::last_turn_cache_pct`]. Called from the `TurnFinished`
    /// handler so the status bar shows a stable value for the turn that just
    /// ended. Sets `None` when the turn had no cache data (no LLM response
    /// reported cache tokens), which makes the segment disappear rather than
    /// render a meaningless `0%`.
    pub fn snapshot_turn_cache_pct(&mut self) {
        let total = self.turn_cache_hit.saturating_add(self.turn_cache_miss);
        if total > 0 {
            self.last_turn_cache_pct = Some((self.turn_cache_hit as f64 / total as f64) * 100.0);
        } else {
            self.last_turn_cache_pct = None;
        }
    }
}

/// State of the currently in-flight turn (if any).
#[derive(Clone, Debug, PartialEq)]
pub struct TurnState {
    pub running: bool,
    pub started_at: Option<Instant>,
    pub spinner_verb: &'static str,
}

impl Default for TurnState {
    fn default() -> Self {
        Self {
            running: false,
            started_at: None,
            spinner_verb: "Thinking",
        }
    }
}

impl TurnState {
    pub fn start(&mut self) {
        self.running = true;
        self.started_at = Some(Instant::now());
        self.spinner_verb = "Thinking";
    }

    pub fn finish(&mut self) {
        self.running = false;
        self.started_at = None;
        self.spinner_verb = "Thinking";
    }
}

// ──────────────────────────────────────────────────────────────────────
// Cost estimation
// ──────────────────────────────────────────────────────────────────────

/// Return the model name to display in the status bar.
///
/// Delegates to `Config::from_env()` so the TUI shows the same model the
/// runtime will actually use — including the `provider.preset` chain added
/// for the preset-config goal. Without this, a user with
/// `provider.preset = "deepseek"` would see "claude-sonnet-4-6" in the
/// status bar while the agent talked to DeepSeek.
pub fn detect_model_name() -> String {
    recursive::config::Config::from_env()
        .map(|c| c.model)
        .unwrap_or_else(|_| "gpt-4o-mini".to_string())
}

/// Return the context-window size (in tokens) for the model the runtime
/// will actually use, so the TUI can show a live "context used / window"
/// gauge. Mirrors [`detect_model_name`] by delegating to
/// `Config::context_window_tokens_effective()`, which honours
/// `context_window_override` and otherwise falls back to the **effective**
/// provider catalog (remote cache + bundled `providers.toml` +
/// `providers.d/` overrides) via `context_window_tokens_for_model_effective`
/// — the same source the `/model` picker lists, so the gauge and the
/// picker agree on the window. Returns a sane non-zero fallback when no
/// config can be loaded.
pub fn detect_context_window() -> u64 {
    context_window_for_model(&detect_model_name())
}

/// Return the effective context-window size (in tokens) for `model`,
/// honouring a global `context_window_override` when set, else the
/// effective provider catalog (remote cache + bundled `providers.toml` +
/// `providers.d/` overrides) via `context_window_tokens_for_model_effective`.
/// This is the single source the TUI context gauge uses -- both at startup
/// (for the config-default model, via [`detect_context_window`]) and on a
/// `/model` hot-swap, so the gauge tracks the live model instead of
/// staying pinned to the startup value. Returns a sane non-zero fallback
/// when no config can be loaded.
pub fn context_window_for_model(model: &str) -> u64 {
    recursive::config::Config::from_env()
        .map(|c| c.context_window_tokens_effective_for(model) as u64)
        .unwrap_or_else(|_| recursive::llm::context_window_tokens_for_model_effective(model) as u64)
}

/// Saturating cast from u64 to u32

/// Saturating cast from u64 to u32: returns `u32::MAX` instead of wrapping.
///
/// `TokenUsage` fields are `u32` but session totals accumulate as `u64`.
/// Very long sessions (>4 billion tokens) are extremely rare in practice,
/// but saturating is safer than silent modular truncation.
fn saturating_u32(v: u64) -> u32 {
    v.min(u32::MAX as u64) as u32
}

/// Compute estimated cost in USD given accumulated tokens and cache stats.
/// Delegates to `pricing_for()` from the bundled provider catalog.
/// Uses `ModelPricing::cost_usd()` which applies cache-hit discount rates.
/// Returns `None` when the model has no pricing entry in `providers.toml`.
pub fn estimate_cost(
    model: &str,
    total_input: u64,
    total_output: u64,
    cache_hit: u64,
    cache_miss: u64,
) -> Option<f64> {
    let pricing = recursive::llm::pricing_for(model)?;
    let usage = recursive::llm::TokenUsage {
        prompt_tokens: saturating_u32(total_input),
        completion_tokens: saturating_u32(total_output),
        total_tokens: 0,
        cache_hit_tokens: saturating_u32(cache_hit),
        cache_miss_tokens: saturating_u32(cache_miss),
        reasoning_tokens: 0,
    };
    Some(pricing.cost_usd(usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_state_finish_clears_running_flag() {
        // kills TurnState::finish -> () (110:9): the mutant leaves
        // `running == true` after finish.
        let mut t = TurnState::default();
        t.start();
        assert!(t.running);
        t.finish();
        assert!(!t.running, "finish should clear running");
        assert!(t.started_at.is_none(), "finish should clear started_at");
    }

    #[test]
    fn record_with_cache_sets_last_prompt_tokens_to_cache_sum_for_anthropic() {
        // Anthropic: input_tokens excludes cached tokens, so the real
        // prompt size is cache_hit + cache_miss (which already folds in
        // input_tokens). last_prompt_tokens must equal the cache sum,
        // not the bare input_tokens.
        let mut u = UsageStats::default();
        u.record_with_cache(
            /*input*/ 150, /*output*/ 50, /*hit*/ 900, /*miss*/ 150,
        );
        assert_eq!(u.last_prompt_tokens, 900 + 150);
    }

    #[test]
    fn record_with_cache_falls_back_to_input_tokens_without_cache_report() {
        // OpenAI without cache reporting: cache fields are 0, input_tokens
        // already carries the full prompt size. last_prompt_tokens must
        // fall back to input_tokens rather than reading 0.
        let mut u = UsageStats::default();
        u.record_with_cache(
            /*input*/ 1234, /*output*/ 50, /*hit*/ 0, /*miss*/ 0,
        );
        assert_eq!(u.last_prompt_tokens, 1234);
    }

    #[test]
    fn detect_context_window_returns_nonzero() {
        // Whatever the configured model is, the resolver must produce a
        // sane non-zero window (the providers.toml fallback guarantees
        // one even for unknown models).
        assert!(detect_context_window() > 0);
    }

    #[test]
    fn context_window_for_model_tracks_named_model() {
        // A catalogued model resolves to a positive window. Used by the
        // `/model` hot-swap handler so the gauge tracks the live model.
        // Machine-independent: a global override only caps the window,
        // never zeroes it, and the bundled fallback covers unknown names.
        assert!(context_window_for_model("deepseek-chat") > 0);
    }
}
