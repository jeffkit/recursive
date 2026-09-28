# Goal 392 — runs_in_flight / transcript_bytes_total gauges (issue #19)

**Date:** 2026-09-28
**Goal:** Add `recursive_runs_in_flight` and `recursive_transcript_bytes_total`
to `/metrics`, per the 02-plan (RAII guard on `RunPermit`; on-demand transcript
scan in `metrics_handler`).

## Files touched
- `src/session_host.rs` — `RunPermit` gains `in_flight: Option<Arc<AtomicU64>>`
  + `Drop` decrement; `AdmissionGate` gains `runs_in_flight` field (both
  constructors take the extra arg); `acquire_run`/`try_acquire_run` bump on
  success only; new `runs_in_flight()` accessor; new test
  `runs_in_flight_raii_guard_covers_early_return`.
- `src/http/mod.rs` — `Metrics.runs_in_flight: Arc<AtomicU64>`; test helper
  constructor updated.
- `src/http/handlers.rs` — `metrics_handler` emits the two new gauge blocks
  (appended after the `recursive_runs_waiting` block); new test
  `metrics_handler_includes_gauges`; ~5 test `AdmissionGate::new` sites
  mechanically updated.
- `src/http/cold_load.rs` — test `AdmissionGate::new` site updated.
- `tests/http.rs` — `metrics_returns_prometheus_format` gains contains
  assertions for both new names; test gate constructors updated.
- `tests/http_common/mod.rs`, `tests/v050_integration.rs`, `tests/agui_e2e.rs`
  — test gate constructors updated (not enumerated in the plan; mechanical
  fallout of the constructor signature change, same class as the planned
  handlers.rs test fixes).
- `crates/recursive-cli/src/main.rs` — production `AdmissionGate::new` call
  site now passes `metrics.runs_in_flight` (this is the real production
  constructor, the plan's "src/http/mod.rs :1523" site is test-only).

## Tests added
- `session_host::tests::runs_in_flight_raii_guard_covers_early_return`
- `http::handlers::tests::metrics_handler_includes_gauges`
- extended `http_tests::metrics_returns_prometheus_format`

## Notes — decisions
- **Journal renamed** from `manual-20260928-goal392-gauges.md` to match the
  acceptance glob.
- **transcript_bytes_skipped counter:** busy sessions skipped by the
  `try_lock()` probe are counted and exposed as
  `recursive_transcript_bytes_skipped` (gauge), closing investigation gap #1 /
  goal Scope §1. Its HELP labels it "Live sessions skipped when sampling
  transcript size (runtime busy)".
- **transcript_bytes_total estimation:** content **character count** sum
  (`m.content.chars().count()`), per the plan's "选 content 字符求和" option.
  The metric name says bytes; the HELP text says "Estimated ... (character
  count; busy sessions skipped)" so scrapers aren't misled. A byte-exact total
  would need `content.len()` — but chars was the plan's chosen trade-off for
  multi-byte safety; noted here for the record. `try_lock()` failure on a
  mid-turn runtime → session skipped (never block the scrape, never lie).
- `runs_in_flight` counts permits held via any of the three acquire sites
  (handlers.rs acquire ×2 / try_acquire ×1) with zero handler changes —
  `?` early returns drop the permit and decrement automatically.

## Verification
- `cargo test --lib metrics` → 3 passed (incl. new gauge test)
- `cargo test --lib runs_in_flight` → 1 passed
- `cargo test --lib session_host` → 17 passed
- `cargo test --test http metrics_returns_prometheus_format` → 1 passed
- `cargo test --test http_cold_load --test agui_e2e` → all passed
- `cargo clippy --all-targets --all-features` → clean
- Live server probe (`recursive http --addr 127.0.0.1:13099`):

```
# HELP recursive_runs_waiting Requests waiting for a run permit
# TYPE recursive_runs_waiting gauge
recursive_runs_waiting 0
# HELP recursive_runs_in_flight Runs currently holding a run permit
# TYPE recursive_runs_in_flight gauge
recursive_runs_in_flight 0
# HELP recursive_transcript_bytes_total Estimated transcript size across live sessions (character count; busy sessions skipped)
# TYPE recursive_transcript_bytes_total gauge
recursive_transcript_bytes_total 0
# HELP recursive_transcript_bytes_skipped Live sessions skipped when sampling transcript size (runtime busy)
# TYPE recursive_transcript_bytes_skipped gauge
recursive_transcript_bytes_skipped 0
```
