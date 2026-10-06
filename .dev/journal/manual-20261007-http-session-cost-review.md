# journal — issue #114 review round (NEEDS_FIX → fixes)

- **Date:** 2026-10-07
- **Goal:** #114 fix(http): HTTP 模式会话成本归零 —— 不落 cost.json/meta、冷加载计数清零、客户端拿不到任何 USD
- **Baseline:** same worktree branch as `manual-20261007-http-session-cost.md`
  (review target `a3897e6d`)

## Files touched (this round)

- `sdk/python/recursive_client/models.py` (**blocker fix**) — `UsageInfo` gained
  the issue-#114 fields with defaults and a `from_dict()` that drops keys the
  client does not know yet; `RunResponse.__post_init__` uses it. The dataclass
  `__init__` previously hard-failed with `TypeError` on the new `/run` payload,
  so `client.run(...)` broke on every successful run.
- `sdk/python/tests/test_client.py` — `test_run` now sends a realistic #114
  payload and asserts the cache split + model + `cost_usd`; new
  `test_run_tolerates_unknown_usage_keys_and_legacy_payloads` pins forward
  compatibility.
- `src/http/handlers.rs` — the session turn's `Err` arm now folds
  `last_failed_usage()` into the session accumulator and persists it (issue
  #115's "systematically low books", at session scope).
- `src/http/agui.rs` — the `/agui` success arm now calls
  `Metrics::record_cost_usd`, so `recursive_cost_usd_total` really is "across
  completed runs" instead of excluding every AG-UI run.
- `src/http/usage.rs` — `SessionUsage` accumulates billed USD per turn
  (`cost_micro_usd`, same micro-USD convention as the global counter) and
  exposes `cost_usd()`; `PersistedUsage` persists that USD instead of the model
  it was priced at; `UsageResponse.cost_usd` is the accumulated figure.
- `src/http/cold_load.rs` — a restored session bills its *new* turns at the
  current server model and restores the frozen USD, so a restart onto another
  model neither re-bills history nor prices new turns at a stale rate.
- `src/http/mod.rs` — OpenAPI descriptions for the endpoint / `UsageResponse`.
- `tests/http.rs` — `session_usage_keeps_a_failed_turns_spend`,
  `agui_run_feeds_the_global_cost_counter`.
- `website/{en,zh}/http-api/run.md` — the `/run` usage breakdown (cache split,
  latency, model, `cost_usd`) and the new `usage` SSE event.
- `website/{en,zh}/http-api/sessions.md` — `GET /sessions/:id/usage`.
- `CHANGELOG.md` — entry under Unreleased.

## Notes

- Design decision (reviewer item 4, "worth confirming"): USD is now summed per
  turn at the model that ran it, and *that sum* is what is persisted. Pricing a
  restored session from the persisted model fixed history but billed future
  turns at a stale rate; pricing it from the current model did the opposite.
  Freezing the accumulated cost does both, and token totals stay untouched so
  the "cold load must not zero the counters" acceptance criterion still holds.
  The accumulator is micro-USD, matching `Metrics::cost_micro_usd_total`
  (≤ 5e-7 USD rounding per turn).
- `UsageResponse.cost_usd` is `None` only while nothing has accrued *and* the
  live model has no pricing entry — a session with real billed history does not
  go back to `null` if the server restarts onto an unpriced model.
- Verified: `cargo test --workspace` (3042 lib + 130 http + 48 invariants, 0
  failed), `cargo clippy --all-targets --all-features -- -D warnings` clean,
  `cargo fmt --all -- --check` clean, `cd sdk/python && python3 -m pytest tests`
  (41 passed).
