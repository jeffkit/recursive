# Issue #123 — liveness vs readiness, active doctor probes, capacity/data-loss signals

- Date: 2026-10-05
- Issue: #123 (P2)
- Baseline: fc29f3d4
- Verdict: completed

## What landed

`/health` was a constant `"ok"` and `doctor` only checked that files existed,
so the three "half-dead" states (dead key/gateway, read-only/full storage,
unwritable disk) all answered 200.

### `GET /healthz` + `GET /readyz` (`src/http/handlers.rs`, `src/http/mod.rs`)

- `/health` unchanged (backward-compatible liveness); `/healthz` is its
  k8s-native alias — both public, no auth, no rate limit.
- `/readyz` runs three checks and returns `200` / `503` with a per-check JSON
  body:
  - **storage** — a real `save_memory` round-trip through `AppState.storage`
    (the same backend teardown saves use). Failure detail is logged, not
    echoed (the endpoint is unauthenticated).
  - **llm** — `llm_failures_consecutive < READYZ_MAX_LLM_FAILURES` (3). One
    transient 5xx does not flap the pod; a hard-broken key/gateway does.
  - **admission** — saturated *and* queueing (`runs_in_flight >=
    max_concurrent && runs_waiting > 0`). Fully-busy-but-draining is normal
    load; `max_concurrent == 0` (unlimited) can never be saturated.
- Routes added to the public sub-router and documented in the OpenAPI spec.

### Metrics (`src/http/mod.rs` `Metrics`, `src/http/handlers.rs`)

- New counters: `last_llm_success_ms`, `llm_failures_consecutive`,
  `persist_failures`, `sessions_evicted`.
- New exposition series: `recursive_sse_clients` (derived from each
  `event_channels` sender's `receiver_count`), `recursive_agui_runs`
  (`agui_active_runs` len), `recursive_persist_failures`,
  `recursive_sessions_evicted`, `recursive_llm_last_success_ms`,
  `recursive_llm_failures_consecutive`.
- `record_run_success` resets the streak and stamps the timestamp;
  `record_run_failed` bumps it.

### Data-loss counting (was `tracing::warn!`-only)

- Reaper eviction save failure → `persist_failures`.
- DELETE transcript save failure and tombstone write failure →
  `persist_failures`.
- Shutdown persistence failure → `persist_failures`; a session still busy at
  graceful shutdown now logs at **error** (`(data loss)`) and counts.

### `recursive doctor --probe` (`crates/recursive-cli/src/main.rs`)

Opt-in active probes after the static checks: one real LLM request
(`probe_llm`), a storage write/read round-trip via `http_storage_backend`
(`probe_storage`), and a 1 MiB disk write/read under `<workspace>/.recursive/`
(`probe_disk`). Any failure flips the existing exit-1 contract.

## Files touched

- `src/http/mod.rs`
- `src/http/handlers.rs`
- `crates/recursive-cli/src/main.rs`
- `tests/http.rs`

## Tests added

- `src/http/handlers.rs`: `readyz_reports_ready_when_checks_pass`,
  `readyz_reports_503_when_llm_streak_reaches_threshold`,
  `readyz_stays_ready_below_llm_failure_threshold`,
  `readyz_reports_503_only_when_saturated_with_waiters`,
  `readyz_reports_503_when_storage_write_fails`,
  `metrics_handler_exposes_capacity_and_loss_series`,
  `record_run_metrics_track_llm_streak`; extended
  `openapi_metrics_path_documents_new_metrics`.
- `src/http/mod.rs`: `evict_counts_persist_failure_when_storage_write_fails`,
  `flush_counts_busy_session_as_data_loss`; extended the eviction / flush
  tests with the new counters.
- `tests/http.rs`: `healthz_returns_ok`, `readyz_reports_ready_with_checks`;
  `/healthz` + `/readyz` added to both auth-exempt loops.
- `crates/recursive-cli/src/main.rs`:
  `doctor_probe_storage_round_trips_and_cleans_up`,
  `doctor_probe_disk_writes_and_cleans_up`.

## Gates

- `cargo test --workspace` — pass
- `cargo clippy --all-targets --all-features -- -D warnings` — pass
- `cargo fmt --all` — clean

## Notes

- Readiness deliberately does **not** gate on sandbox reachability: creating a
  container per probe would be too heavy. Doctor's `--probe` covers the local
  key/storage/disk states, and per-session sandbox failures already surface as
  per-request 503s.
- `/readyz` writes a reserved `__readyz_probe__` key; that is the point (it is
  the only way to see a read-only/full backend), and the write is tiny and
  idempotent. Since the review round it is also cached for
  `READYZ_PROBE_TTL_MS` and read back.

## Review round (NEEDS_FIX → fixes)

Independent review flagged one blocking test and two `/readyz` design defects.

### Blocking: order-dependent `record_run_metrics_track_llm_streak`

`now_session_ms()` returns `0` for the first caller in a process (lazy
`SESSION_EPOCH`), so a success landing in that first millisecond was
indistinguishable from "never succeeded" — and the test only passed when
another test warmed the epoch first. Fixed at the source: every readiness
stamp goes through `handlers::now_stamp_ms()`, which reserves `0` for the
"never happened" sentinel (`Metrics::last_llm_success_ms` / `last_llm_failure_ms`
/ `readyz_storage_probed_ms`). The `> 0` assertions now hold running alone.

### `/readyz` LLM check: one-way latch

`llm_failures_consecutive >= READYZ_MAX_LLM_FAILURES` was cleared only by a
successful run — but a pod tripped out of a Service's endpoints receives no
runs, so a transient blip latched until a restart. Added
`READYZ_LLM_FAILURE_WINDOW_MS` (60 s) + `Metrics::last_llm_failure_ms`:
`/readyz` decays a streak whose most recent failure is older than the window
(compare-and-swap, so a concurrent failure is not lost) and reports ready
again. A genuinely broken gateway re-trips within 3 requests.

### `/readyz` LLM check: non-LLM failures counted as LLM failures

`record_run_failed` bumped the streak for *any* run failure, including AG-UI
client cancellations. Split the signals:

- `record_run_success` / `record_run_failed` — run counters only.
- `record_llm_success` / `record_llm_failure` — readiness signal, called only
  where the run really completed (or `Error::is_llm_failure()` holds: a
  revoked key, an unreachable gateway, a malformed provider response; a 429
  means the endpoint answered and does not count).
- A cancelled turn (AG-UI cancel, session interrupt) now touches neither.

### Storage probe: real round-trip, shared, and bounded

- `storage::memory_round_trip()` — one write+read-back implementation used by
  both `/readyz` and `recursive doctor --probe`, which previously differed
  (`/readyz` only wrote). The docs claiming a "write round-trip" are now true.
- `READYZ_PROBE_TTL_MS` (5 s) + `Metrics::readyz_storage_{probed_ms,ok}`: the
  verdict is cached, so scraping this unauthenticated route can no longer
  drive one storage write (an S3 PUT in cloud deployments) per request.
- The probe value is process-stable, so two concurrent probes cannot make each
  other's read-back look like a mismatch.

### Tests added by the fix round

`readyz_decays_a_stale_llm_streak_instead_of_latching`,
`llm_streak_is_current_only_while_the_last_failure_is_recent`,
`readyz_reports_503_when_the_storage_round_trip_is_lost`,
`readyz_caches_the_storage_verdict_within_the_ttl`,
`readyz_probe_is_fresh_only_inside_the_ttl` (handlers),
`memory_round_trip_{accepts_a_working_backend,rejects_a_refused_write,rejects_a_dropped_write}`
(storage), plus the rewritten `record_run_metrics_track_llm_streak` and the
stamped-threshold updates.
