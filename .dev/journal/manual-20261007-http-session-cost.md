# journal — issue #114 HTTP 会话成本归零

- **Date:** 2026-10-07
- **Goal:** #114 fix(http): HTTP 模式会话成本归零 —— 不落 cost.json/meta、冷加载计数清零、客户端拿不到任何 USD
- **Baseline:** origin/main = fc29f3d4（worktree branch `v2-pipeline-114-1007033305`）

## Files touched

- `src/http/usage.rs` (**new**) — per-session accounting: `SessionUsage` (lock-free atomics
  for prompt / completion / cache-hit / cache-miss / reasoning / total / llm latency + the
  priced model), `UsageTotals` snapshot, `cost_usd()` via `llm::pricing_for` +
  `ModelPricing::cost_usd` (same function `CostTracker` bills with), `UsageResponse`
  (`GET /sessions/:id/usage`), and best-effort persistence helpers
  (`usage_key` / `persist_usage` / `load_persisted_usage`) over the storage backends'
  generic key/value space (`session-usage/<id>`).
- `src/http/mod.rs` — `SessionState.prompt_tokens`/`completion_tokens` replaced by
  `usage: Arc<SessionUsage>`; `Metrics.cost_micro_usd_total` + `record_cost_usd` /
  `cost_usd_total`; `UsageInfo` now carries the cache split + model + USD
  (`UsageInfo::from_turn`); new `SseEvent::Usage`; route `/sessions/{id}/usage`;
  OpenAPI path + `UsageResponse` schema + metrics description.
- `src/http/handlers.rs` — `get_session_usage` handler (`GET /sessions/:id/usage`);
  `create_session`/`fork_session` build the usage accumulator; `get_session` reads the
  snapshot; `send_session_message` records + persists usage and bumps the USD counter;
  `run_agent` reports the full `UsageInfo` + bumps the counter; `map_agent_event` forwards
  the per-step `AgentEvent::Usage` (was silently dropped); `/metrics` exposes
  `recursive_cost_usd_total`.
- `src/http/cold_load.rs` — load the persisted usage snapshot and restore the counters +
  model, so a restart no longer zeroes a session's usage.
- `tests/http.rs` — end-to-end: usage endpoint reports the cache split + USD, survives a
  graceful-shutdown restart, is owner-scoped (403/404), and `POST /run` reports cache
  split + USD + moves the global counter.

## Tests added

Unit (`src/http/usage.rs`): counter independence/accumulation/restore, `from_token_usage`,
pricing exactness + cache discount + parity with `CostTracker`, unknown-model → `None`,
u32 saturation, KV roundtrip, zero-blob suppression, missing/corrupt/IO-error degradation,
storage-failure tolerance, `UsageResponse` shape.

Unit (`src/http/handlers.rs` / `mod.rs` / `cold_load.rs`): `record_cost_usd` (priced only,
sub-micro rounding, >$1 division), `/metrics` exposure, `map_agent_event` forwarding,
OpenAPI path/schema, cold-load restore + legacy zero-start.

## Notes

- Design choice: usage rides the `StorageBackend` key/value space instead of a local
  `cost.json`. The HTTP channel may be backed by S3/Redis, so a filesystem `cost.json`
  would not exist there — the KV blob gets the same durability for every backend.
- USD is computed from the identical function pair the CLI/AG-UI `CostTracker` uses, so
  the numbers agree across channels (pinned by `cost_usd_matches_cost_tracker`).
- The metric stores micro-USD in an `AtomicU64` (no atomics for `f64`) and renders the
  float counter `recursive_cost_usd_total`.
- Verified: `cargo test --workspace` (4504 passed, 0 failed), `cargo clippy --all-targets
  --all-features -- -D warnings` clean, `cargo fmt --all -- --check` clean.
