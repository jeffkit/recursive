//! Labelled counters, histograms and the run-event sink behind `/metrics`
//! (issue #113).
//!
//! The pre-#113 collector was a flat set of `AtomicU64`s, which cannot answer
//! the questions an operator actually asks: *which* route returns 5xx, *which*
//! model burns the budget, *how long* a run waited for an admission permit,
//! and *why* a run stopped (`/run` reported `"status": "success"` for a
//! `budget_exceeded` termination). Every dimension that is not a scalar total
//! now lives in one of the families below, rendered in Prometheus text format
//! by [`CounterFamily::render`] / [`HistogramFamily::render`].
//!
//! Cardinality is bounded by construction: label values are either fixed
//! vocabularies ([`finish_reason_label`], the runtime's retry reasons), the
//! matched route template (`/sessions/{id}/messages`, never a session id),
//! the configured model, or a registry tool name.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::agent::FinishReason;
use crate::event::{AgentEvent, EventSink};

use super::Metrics;

/// `finish_reason` label for a run that ended with an `Err` — an error has no
/// [`FinishReason`] (invariant #7: finish reasons are data, errors are not).
pub const FINISH_REASON_ERROR: &str = "error";

/// Bounded `finish_reason` label for a [`FinishReason`].
///
/// The dynamic payloads (`provider_stop:<reason>`,
/// `stuck:<call>:<repeats>`, `transcript_limit:<chars>/<limit>`,
/// `wall_clock_exceeded:<secs>`) are dropped: the label identifies the
/// *variant*, so the series count stays bounded by the enum rather than by
/// whatever text the provider or the transcript happened to carry.
pub fn finish_reason_label(reason: &FinishReason) -> &'static str {
    match reason {
        FinishReason::NoMoreToolCalls => "no_more_tool_calls",
        FinishReason::BudgetExceeded => "budget_exceeded",
        FinishReason::ProviderStop(_) => "provider_stop",
        FinishReason::Stuck { .. } => "stuck",
        FinishReason::TranscriptLimit { .. } => "transcript_limit",
        FinishReason::Cancelled => "cancelled",
        FinishReason::PermissionDenialLimit => "permission_denial_limit",
        FinishReason::WallClockExceeded { .. } => "wall_clock_exceeded",
    }
}

/// `POST /run`'s `status` field for a finish reason.
///
/// `"success"` is reserved for [`FinishReason::NoMoreToolCalls`] — the model
/// answered. Every other termination is *data, not an error* (invariant #7),
/// but it is not a success either: it is reported under its own bounded label
/// so a caller can tell a budget stop from a stuck loop without parsing the
/// free-form `finish_reason` string.
pub fn run_status_label(reason: &FinishReason) -> &'static str {
    match reason {
        FinishReason::NoMoreToolCalls => "success",
        other => finish_reason_label(other),
    }
}

/// Prometheus text-format escaping for a label value (`\\`, `\"`, `\n`).
fn push_escaped(out: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(ch),
        }
    }
}

/// Render `{a="1",b="2"}`, or nothing when there are no label pairs.
fn render_label_set(pairs: &[(&str, &str)]) -> String {
    if pairs.is_empty() {
        return String::new();
    }
    let mut out = String::from("{");
    for (i, (name, value)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(name);
        out.push_str("=\"");
        push_escaped(&mut out, value);
        out.push('"');
    }
    out.push('}');
    out
}

/// A counter with a fixed set of label names, keyed by label values.
///
/// Series are created on first use: a family nothing has incremented renders
/// no series at all. Bounded cardinality is the caller's responsibility — see
/// the module docs for the label vocabularies in use.
#[derive(Default)]
pub struct CounterFamily {
    series: Mutex<BTreeMap<Vec<String>, u64>>,
}

impl CounterFamily {
    /// Add `value` to the series identified by `labels` (saturating).
    pub fn add(&self, labels: &[&str], value: u64) {
        let key: Vec<String> = labels.iter().map(|v| (*v).to_string()).collect();
        let mut series = self.series.lock().unwrap_or_else(|e| e.into_inner());
        let entry = series.entry(key).or_insert(0);
        *entry = entry.saturating_add(value);
    }

    /// Increment the series identified by `labels` by one.
    pub fn inc(&self, labels: &[&str]) {
        self.add(labels, 1);
    }

    /// Sum across every series (saturating). `0` for an untouched family.
    pub fn total(&self) -> u64 {
        self.series
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .copied()
            .fold(0u64, u64::saturating_add)
    }

    /// Every series, ordered by label values.
    pub fn snapshot(&self) -> Vec<(Vec<String>, u64)> {
        self.series
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(labels, value)| (labels.clone(), *value))
            .collect()
    }

    /// Render as `# HELP` / `# TYPE counter` plus one line per series.
    ///
    /// `label_names` names the values every caller passed to [`Self::add`],
    /// in the same order.
    pub fn render(&self, name: &str, help: &str, label_names: &[&str]) -> String {
        self.render_with(name, help, label_names, None)
    }

    /// Render as a float counter: each value divided by `divisor` and printed
    /// with 6 decimals. Used by the micro-USD cost family so the exposition
    /// reports dollars, not micro-dollars.
    pub fn render_scaled(
        &self,
        name: &str,
        help: &str,
        label_names: &[&str],
        divisor: f64,
    ) -> String {
        self.render_with(name, help, label_names, Some(divisor))
    }

    fn render_with(
        &self,
        name: &str,
        help: &str,
        label_names: &[&str],
        divisor: Option<f64>,
    ) -> String {
        let mut out = format!("# HELP {name} {help}\n# TYPE {name} counter\n");
        for (labels, value) in self.snapshot() {
            let pairs: Vec<(&str, &str)> = label_names
                .iter()
                .copied()
                .zip(labels.iter().map(String::as_str))
                .collect();
            out.push_str(name);
            out.push_str(&render_label_set(&pairs));
            match divisor {
                Some(divisor) => out.push_str(&format!(" {:.6}\n", value as f64 / divisor)),
                None => {
                    out.push(' ');
                    out.push_str(&value.to_string());
                    out.push('\n');
                }
            }
        }
        out
    }
}

/// One histogram series: per-bound observation counts plus the running sum and
/// count. Bucket counts are stored **non-cumulatively**; `_bucket` lines are
/// made cumulative at render time.
struct HistogramSeries {
    /// One count per configured bound, plus the `+Inf` overflow bucket.
    buckets: Vec<u64>,
    sum: u64,
    count: u64,
}

impl HistogramSeries {
    fn new(bounds: usize) -> Self {
        Self {
            buckets: vec![0; bounds + 1],
            sum: 0,
            count: 0,
        }
    }
}

/// Immutable view of one histogram series (for tests and custom renderers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistogramSnapshot {
    /// Label values, in the order the caller passed them.
    pub labels: Vec<String>,
    /// Non-cumulative observation counts: one per bound, then the overflow.
    pub buckets: Vec<u64>,
    pub sum: u64,
    pub count: u64,
}

/// A histogram with static upper bounds and integer observations.
///
/// Every `/metrics` histogram measures whole milliseconds or whole steps, so
/// observations and the per-series sum are `u64` — no float atomics, no
/// precision loss, and no metrics dependency.
pub struct HistogramFamily {
    bounds: &'static [u64],
    series: Mutex<BTreeMap<Vec<String>, HistogramSeries>>,
}

impl HistogramFamily {
    /// Build a family with the ascending upper bounds `bounds`.
    pub fn new(bounds: &'static [u64]) -> Self {
        Self {
            bounds,
            series: Mutex::new(BTreeMap::new()),
        }
    }

    /// The configured upper bounds.
    pub fn bounds(&self) -> &'static [u64] {
        self.bounds
    }

    /// Record one observation under `labels`.
    pub fn observe(&self, labels: &[&str], value: u64) {
        let key: Vec<String> = labels.iter().map(|v| (*v).to_string()).collect();
        let bounds = self.bounds;
        let mut series = self.series.lock().unwrap_or_else(|e| e.into_inner());
        let entry = series
            .entry(key)
            .or_insert_with(|| HistogramSeries::new(bounds.len()));
        let idx = bounds
            .iter()
            .position(|b| value <= *b)
            .unwrap_or(bounds.len());
        entry.buckets[idx] = entry.buckets[idx].saturating_add(1);
        entry.sum = entry.sum.saturating_add(value);
        entry.count = entry.count.saturating_add(1);
    }

    /// Every series, ordered by label values.
    pub fn snapshot(&self) -> Vec<HistogramSnapshot> {
        self.series
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(labels, s)| HistogramSnapshot {
                labels: labels.clone(),
                buckets: s.buckets.clone(),
                sum: s.sum,
                count: s.count,
            })
            .collect()
    }

    /// Render as `# HELP` / `# TYPE histogram`, the cumulative `_bucket` lines
    /// (each carrying the family's labels plus `le`), `_sum` and `_count`.
    pub fn render(&self, name: &str, help: &str, label_names: &[&str]) -> String {
        let mut out = format!("# HELP {name} {help}\n# TYPE {name} histogram\n");
        for series in self.snapshot() {
            let base: Vec<(&str, &str)> = label_names
                .iter()
                .copied()
                .zip(series.labels.iter().map(String::as_str))
                .collect();
            let mut cumulative = 0u64;
            for (i, bound) in self.bounds.iter().enumerate() {
                cumulative = cumulative.saturating_add(series.buckets[i]);
                let le = bound.to_string();
                let mut pairs = base.clone();
                pairs.push(("le", &le));
                out.push_str(name);
                out.push_str("_bucket");
                out.push_str(&render_label_set(&pairs));
                out.push(' ');
                out.push_str(&cumulative.to_string());
                out.push('\n');
            }
            cumulative = cumulative.saturating_add(series.buckets[self.bounds.len()]);
            let mut pairs = base.clone();
            pairs.push(("le", "+Inf"));
            out.push_str(name);
            out.push_str("_bucket");
            out.push_str(&render_label_set(&pairs));
            out.push(' ');
            out.push_str(&cumulative.to_string());
            out.push('\n');
            out.push_str(name);
            out.push_str("_sum");
            out.push_str(&render_label_set(&base));
            out.push(' ');
            out.push_str(&series.sum.to_string());
            out.push('\n');
            out.push_str(name);
            out.push_str("_count");
            out.push_str(&render_label_set(&base));
            out.push(' ');
            out.push_str(&series.count.to_string());
            out.push('\n');
        }
        out
    }
}

/// [`EventSink`] that folds run events into `/metrics` (issue #113).
///
/// Wired next to — never instead of — the transport sinks of `/run`,
/// `/sessions/:id/messages` and `/agui`. Everything it counts (tool errors,
/// LLM retries, compaction attempts) happens inside the runtime, so no handler
/// can observe it directly; before this sink those three dimensions were
/// simply absent from the exposition.
pub struct MetricsSink {
    metrics: Arc<Metrics>,
}

impl MetricsSink {
    /// Build a sink that folds into `metrics`.
    pub fn new(metrics: Arc<Metrics>) -> Self {
        Self { metrics }
    }
}

#[async_trait::async_trait]
impl EventSink for MetricsSink {
    async fn emit(&self, event: AgentEvent) {
        match event {
            AgentEvent::ToolResult { name, is_error, .. } => {
                if is_error {
                    self.metrics.tool_errors.inc(&[&name]);
                }
            }
            AgentEvent::LlmRetry { reason, .. } => {
                self.metrics.llm_retries.inc(&[&reason]);
            }
            AgentEvent::Compacted { .. } => {
                self.metrics.compactions.inc(&["summary"]);
            }
            AgentEvent::Microcompact { pruned, .. } => {
                if pruned > 0 {
                    self.metrics.compactions.inc(&["micro"]);
                }
            }
            AgentEvent::CompactionBoundary { .. } => {
                self.metrics.compactions.inc(&["boundary"]);
            }
            AgentEvent::CompactionSkipped { reason, .. } => {
                self.metrics.compaction_skipped.inc(&[match reason {
                    crate::event::CompactionSkipReason::CircuitBreaker => "circuit_breaker",
                    crate::event::CompactionSkipReason::Error => "error",
                }]);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_family_sums_and_renders_label_sets() {
        let family = CounterFamily::default();
        family.inc(&["/health", "200"]);
        family.add(&["/run", "503"], 4);
        family.inc(&["/health", "200"]);
        assert_eq!(family.total(), 6);
        // A fresh family renders no series (only HELP/TYPE).
        let empty = CounterFamily::default();
        let rendered = empty.render("m", "help", &["route", "status"]);
        assert_eq!(rendered, "# HELP m help\n# TYPE m counter\n");

        let rendered = family.render("m", "help", &["route", "status"]);
        assert!(
            rendered.contains("m{route=\"/health\",status=\"200\"} 2"),
            "{rendered}"
        );
        // BTreeMap ordering makes the exposition deterministic.
        let health = rendered.find("/health").expect("health series");
        let run = rendered.find("/run").expect("run series");
        assert!(health < run, "series must be ordered: {rendered}");
    }

    #[test]
    fn counter_family_escapes_label_values() {
        let family = CounterFamily::default();
        family.inc(&["a\"b\\c\nd"]);
        let rendered = family.render("m", "h", &["route"]);
        assert!(
            rendered.contains("m{route=\"a\\\"b\\\\c\\nd\"} 1"),
            "label values must be escaped: {rendered}"
        );
    }

    #[test]
    fn counter_family_renders_scaled_floats() {
        let family = CounterFamily::default();
        family.add(&["deepseek-chat"], 280_000);
        let rendered = family.render_scaled("cost", "h", &["model"], 1_000_000.0);
        assert!(
            rendered.contains("cost{model=\"deepseek-chat\"} 0.280000"),
            "{rendered}"
        );
    }

    #[test]
    fn histogram_buckets_are_cumulative_and_overflow_to_inf() {
        let family = HistogramFamily::new(&[10, 100]);
        family.observe(&["m"], 5);
        family.observe(&["m"], 10);
        family.observe(&["m"], 50);
        family.observe(&["m"], 5_000);
        let snap = family.snapshot();
        assert_eq!(snap.len(), 1);
        // 5 and 10 land in le=10; 50 in le=100; 5000 overflows to +Inf.
        assert_eq!(snap[0].buckets, vec![2, 1, 1]);
        assert_eq!(snap[0].sum, 5_065);
        assert_eq!(snap[0].count, 4);

        let rendered = family.render("lat", "h", &["model"]);
        assert!(
            rendered.contains("lat_bucket{model=\"m\",le=\"10\"} 2"),
            "{rendered}"
        );
        assert!(
            rendered.contains("lat_bucket{model=\"m\",le=\"100\"} 3"),
            "{rendered}"
        );
        assert!(
            rendered.contains("lat_bucket{model=\"m\",le=\"+Inf\"} 4"),
            "{rendered}"
        );
        assert!(rendered.contains("lat_sum{model=\"m\"} 5065"), "{rendered}");
        assert!(rendered.contains("lat_count{model=\"m\"} 4"), "{rendered}");
    }

    /// A value exactly on a bound belongs to that bucket (`le` is inclusive) —
    /// a `<` comparison would shift every boundary observation one bucket up.
    #[test]
    fn histogram_bound_is_inclusive() {
        let family = HistogramFamily::new(&[10]);
        family.observe(&[], 10);
        assert_eq!(family.snapshot()[0].buckets, vec![1, 0]);
    }

    #[test]
    fn histogram_without_observations_renders_no_series() {
        let family = HistogramFamily::new(&[10]);
        assert_eq!(
            family.render("m", "h", &["model"]),
            "# HELP m h\n# TYPE m histogram\n"
        );
        assert_eq!(family.bounds(), &[10]);
    }

    #[test]
    fn finish_reason_labels_are_bounded() {
        assert_eq!(
            finish_reason_label(&FinishReason::NoMoreToolCalls),
            "no_more_tool_calls"
        );
        assert_eq!(
            finish_reason_label(&FinishReason::BudgetExceeded),
            "budget_exceeded"
        );
        assert_eq!(
            finish_reason_label(&FinishReason::ProviderStop("content_filter".into())),
            "provider_stop",
            "the dynamic reason must not become a label"
        );
        assert_eq!(
            finish_reason_label(&FinishReason::Stuck {
                repeated_call: "bash".into(),
                repeats: 3
            }),
            "stuck"
        );
        assert_eq!(
            finish_reason_label(&FinishReason::TranscriptLimit { chars: 1, limit: 2 }),
            "transcript_limit"
        );
        assert_eq!(finish_reason_label(&FinishReason::Cancelled), "cancelled");
        assert_eq!(
            finish_reason_label(&FinishReason::PermissionDenialLimit),
            "permission_denial_limit"
        );
        assert_eq!(
            finish_reason_label(&FinishReason::WallClockExceeded { secs: 30 }),
            "wall_clock_exceeded"
        );
    }

    #[test]
    fn run_status_is_success_only_for_a_natural_finish() {
        assert_eq!(run_status_label(&FinishReason::NoMoreToolCalls), "success");
        assert_eq!(
            run_status_label(&FinishReason::BudgetExceeded),
            "budget_exceeded",
            "a budget stop must not be reported as a success"
        );
        assert_eq!(run_status_label(&FinishReason::Cancelled), "cancelled");
    }

    #[tokio::test]
    async fn metrics_sink_counts_tool_errors_retries_and_compactions() {
        let metrics = Arc::new(Metrics::default());
        let sink = MetricsSink::new(Arc::clone(&metrics));

        sink.emit(AgentEvent::ToolResult {
            id: "1".into(),
            name: "Bash".into(),
            output: "ERROR: boom".into(),
            step: 1,
            is_error: true,
            duration_ms: 3,
        })
        .await;
        // A successful tool result must not be counted.
        sink.emit(AgentEvent::ToolResult {
            id: "2".into(),
            name: "Bash".into(),
            output: "ok".into(),
            step: 1,
            is_error: false,
            duration_ms: 3,
        })
        .await;
        sink.emit(AgentEvent::LlmRetry {
            step: 2,
            attempt: 1,
            wait_ms: 500,
            status: Some(429),
            reason: "rate_limited".into(),
        })
        .await;
        sink.emit(AgentEvent::Compacted {
            removed: 3,
            kept: 1,
            summary_chars: 10,
            step: 3,
        })
        .await;
        // pruned > 0 is a real pruning; pruned == 0 is a no-op.
        sink.emit(AgentEvent::Microcompact { step: 4, pruned: 2 })
            .await;
        sink.emit(AgentEvent::Microcompact { step: 5, pruned: 0 })
            .await;
        sink.emit(AgentEvent::CompactionSkipped {
            step: 6,
            reason: crate::event::CompactionSkipReason::CircuitBreaker,
        })
        .await;

        assert_eq!(
            metrics.tool_errors.snapshot(),
            vec![(vec!["Bash".to_string()], 1)]
        );
        assert_eq!(
            metrics.llm_retries.snapshot(),
            vec![(vec!["rate_limited".to_string()], 1)]
        );
        assert_eq!(
            metrics.compactions.snapshot(),
            vec![
                (vec!["micro".to_string()], 1),
                (vec!["summary".to_string()], 1)
            ]
        );
        assert_eq!(
            metrics.compaction_skipped.snapshot(),
            vec![(vec!["circuit_breaker".to_string()], 1)]
        );
    }
}
