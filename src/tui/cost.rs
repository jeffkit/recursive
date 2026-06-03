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
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsageStats {
    /// Most recent per-turn input tokens.
    pub input_tokens: u64,
    /// Most recent per-turn output tokens.
    pub output_tokens: u64,
    /// Cumulative input tokens across all turns.
    pub total_input: u64,
    /// Cumulative output tokens across all turns.
    pub total_output: u64,
    /// Most recent LLM round-trip latency, in milliseconds.
    pub last_latency_ms: u64,
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
    crate::config::Config::from_env()
        .map(|c| c.model)
        .unwrap_or_else(|_| "gpt-4o-mini".to_string())
}

/// Compute estimated cost in USD given accumulated tokens.
/// Delegates to `pricing_for()` from the bundled provider catalog.
/// Returns `None` when the model has no pricing entry in `providers.toml`.
pub fn estimate_cost(model: &str, total_input: u64, total_output: u64) -> Option<f64> {
    let pricing = crate::llm::pricing_for(model)?;
    let in_cost = (total_input as f64) * pricing.input_per_million / 1_000_000.0;
    let out_cost = (total_output as f64) * pricing.output_per_million / 1_000_000.0;
    Some(in_cost + out_cost)
}
