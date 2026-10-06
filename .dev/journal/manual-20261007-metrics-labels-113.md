# Manual journal — issue #113 (HTTP `/metrics`: labels, histograms, finish reasons)

Date: 2026-10-07
Goal: #113 — `/metrics` had no labels, no histograms and no cost dimension: a
per-route 5xx rate could not be computed, per-model spend was invisible,
`BudgetExceeded` was reported as `"status":"success"`, and compaction / retry /
tool-error activity left no trace.

## Files touched

- `src/http/metrics.rs` (new) — `CounterFamily` (labelled counter) and
  `HistogramFamily` (static bounds, integer observations) with Prometheus text
  rendering + escaping; `finish_reason_label` (bounded vocabulary),
  `run_status_label`, `FINISH_REASON_ERROR`; `MetricsSink`, an `EventSink` that
  folds tool errors / LLM retries / compactions into `/metrics`.
- `src/http/mod.rs` — `Metrics` gains `requests_by_route`,
  `agent_runs_finished`, `cost_micro_usd_by_model`, `tool_errors`,
  `llm_retries`, `compactions`, `compaction_skipped`, `llm_latency_ms`,
  `run_steps`, `admission_wait_ms`; `Default` is now hand-written so the
  histogram bucket ladders are configured. `cost_micro_usd_total: AtomicU64`
  became the per-model family (one source of truth; `cost_usd_total()` sums it).
  `RunResponse` / OpenAPI docs updated for the finish-reason-derived `status`.
- `src/http/rate_limit.rs` — `metrics_middleware` reads axum `MatchedPath`
  (route *template*, not the concrete path) and the response status and records
  `recursive_requests_total{route,status}`.
- `src/http/handlers.rs` — `record_run_success(metrics, model, outcome)` /
  `record_run_failed(metrics, usage, Option<&FinishReason>)`; `/run` `status`
  now comes from `run_status_label`; `acquire_run_timed` observes the admission
  wait; `MetricsSink` attached to `/run` and to the session composite sink;
  exposition renders the new families.
- `src/http/agui.rs` — AG-UI fan-out includes `MetricsSink`; the three
  `record_run_*` call sites pass the model / finish reason.
- `src/http/triggers.rs` — trigger runs go through `acquire_run_timed` too.
- `tests/http.rs` — `metrics_returns_prometheus_format` lists the new series;
  `metrics_middleware_increments_requests_total` pins the route/status labels
  and their rendering; `run_with_custom_max_steps_respected` now expects
  `status == "budget_exceeded"`.

## Tests added

- `src/http/metrics.rs`: label-set summing + deterministic ordering, label-value
  escaping, float rendering (micro-USD → USD), histogram cumulative buckets /
  `+Inf` overflow / inclusive bounds / empty family, bounded finish-reason
  labels, `run_status_label`, and a `MetricsSink` test covering tool errors,
  retries, compactions and skipped compactions.
- `src/http/handlers.rs`: `record_run_success_files_finish_reason_steps_and_latency`
  (incl. `budget_exceeded`), `acquire_run_timed_observes_admitted_and_timed_out_waits`,
  cost-per-model exposition, and the `error` sentinel in the existing
  run-metrics test.

## Notes

- No new dependency: the families are `Mutex<BTreeMap<…>>` + `u64`, which is
  what a hand-rolled Prometheus text endpoint needs and keeps. Cardinality is
  bounded by construction (route template, model, tool name, fixed
  vocabularies); dynamic finish-reason payloads are dropped from the label.
- `agent_runs_success` semantics are unchanged (every `Ok` outcome — a finish
  reason is data, not an error, invariant #7); the new
  `recursive_agent_runs_finished_total{finish_reason}` is what makes a
  `budget_exceeded` stop visible.
- Unmatched requests are labelled `route="unmatched"`; the labelled
  `recursive_requests_total` series appear from the second scrape onwards
  because the middleware records the status after the response exists.
- Runtime visibility of the same dimensions via Langfuse is #124 (out of scope).
