//! Token usage accounting and cost estimation for the Recursive TUI.
//!
//! Contains [`UsageStats`] for per-session token accumulation, [`TurnState`]
//! for in-flight turn tracking, and helpers to detect the active model and
//! estimate dollar cost from token counts.

use std::collections::HashMap;
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
// Pricing table
// ──────────────────────────────────────────────────────────────────────

/// (input_per_1k, output_per_1k) USD prices for the four models the
/// goal explicitly calls out. Models not in this table render no `$…`
/// segment in the status bar.
pub fn default_pricing_table() -> HashMap<&'static str, (f64, f64)> {
    let mut m = HashMap::new();
    // Source: published list prices as of mid-2025; not authoritative.
    m.insert("deepseek-chat", (0.00027, 0.00110));
    m.insert("gpt-4o", (0.00250, 0.01000));
    m.insert("gpt-4o-mini", (0.00015, 0.00060));
    m.insert("glm-4-plus", (0.00050, 0.00150));
    m.insert("claude-sonnet", (0.00300, 0.01500));
    m
}

/// Return the model name to display in the status bar.
///
/// Honours the same priority chain `Backend::build_runtime` does:
/// `RECURSIVE_MODEL` / `OPENAI_MODEL` env vars, then
/// `~/.recursive/config.toml`'s `[provider].model`, then the
/// hardcoded `gpt-4o-mini` default. Without this fallback the
/// status bar would show "gpt-4o-mini" even when the runtime is
/// actually talking to DeepSeek/etc.
pub fn detect_model_name() -> String {
    if let Ok(m) = std::env::var("RECURSIVE_MODEL") {
        return m;
    }
    if let Ok(m) = std::env::var("OPENAI_MODEL") {
        return m;
    }
    if let Ok(Some(cfg)) = crate::config_file::FileConfig::load() {
        if let Some(m) = cfg.provider.and_then(|p| p.model) {
            if !m.is_empty() {
                return m;
            }
        }
    }
    "gpt-4o-mini".to_string()
}

/// Compute estimated cost in USD given accumulated tokens and a
/// pricing table. Returns `None` when the model is not known.
pub fn estimate_cost(
    model: &str,
    total_input: u64,
    total_output: u64,
    pricing: &HashMap<&'static str, (f64, f64)>,
) -> Option<f64> {
    pricing.get(model).map(|(in_rate, out_rate)| {
        (total_input as f64) / 1000.0 * in_rate + (total_output as f64) / 1000.0 * out_rate
    })
}
