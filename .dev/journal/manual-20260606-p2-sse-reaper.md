# Manual edit: p2-sse-reaper

**Date**: 2026-06-06
**Goal**: Fix two P2 issues — SSE connections never closing (B2) and session reaper not running (M3)
**Files touched**:
- `src/http/handlers.rs` — B2: add 30s heartbeat comment stream merged with agent events, plus 1-hour max-lifetime timeout on the SSE session
- `src/main.rs` — M3: spawn `session_reaper` task (60s check interval) on HTTP server startup, before `axum::serve`
- `src/runtime.rs` — remove top-level unused import of `ENTER_PLAN_MODE_TOOL_NAME`/`EXIT_PLAN_MODE_TOOL_NAME` (constants are only used inside `#[cfg(test)]`, which has its own local import); fixes pre-existing clippy `-D warnings` failure

**Tests added**: none (covered by existing 79 HTTP integration tests)

**Notes**:
- Heartbeat is an SSE comment (`: heartbeat`) produced by `IntervalStream` merged with the `BroadcastStream` of agent events.
- The merged stream is wrapped in `.timeout(Duration::from_secs(3600))` so connections are bounded to 1 hour even if the client stays connected silently.
- The reaper clone uses `Arc::new(state.clone())` — `AppState` derives `Clone` and all interior collections are already `Arc`-wrapped so no data is duplicated.
- `spawn_session_reaper` is already `pub` in `src/http/mod.rs`; no API surface change needed.
