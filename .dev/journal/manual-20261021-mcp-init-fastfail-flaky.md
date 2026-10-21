# 2026-10-21 — fix(test): mcp http_initialize fast-fail bound flakes under parallel load

## Date
2026-10-21

## Goal
`cargo test --workspace` failed once: `mcp::tests::http_initialize_fails_fast_and_ignores_the_tool_budget` measured 903.98ms against a 900ms bound (issue #181 tiering check). The client failed via the correct 300ms read-budget tier, but full-workspace parallel execution (heavy tests like `concurrent_same_worker_id_spawn_admits_at_most_one` running >60s concurrently) delayed the timed-out future's wakeup past the assertion bound. Idle, the test measures ~300–310ms — the tiering logic itself is sound.

## Files touched
- `src/mcp.rs` (test only): mock `initialize` delay 1.2s → 3s, tool budget 5s → 10s, latency bound 900ms → 2s. The delays now strictly bracket the bounds: a pass (error < 2s) is only reachable via the read-timeout tier; regression to the tool budget or the 30s client backstop returns `Ok` at 3s and trips the first assert. Same invariant, robust to scheduler jitter.

## Tests added
None — existing test made deterministic.

## Notes
- Root cause was assertion tolerance vs. load jitter, not `http_request_timeout` tiering; no product logic changed.
- Verified: `cargo test --workspace` → 69 targets, all `0 failed`; `cargo fmt --all -- --check` clean; `cargo clippy -p recursive-agent --lib -- -D warnings` clean.
