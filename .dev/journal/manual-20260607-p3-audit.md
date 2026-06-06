# Manual edit: p3-audit

**Date**: 2026-06-07
**Goal**: Audit and fix all P3 technical debt items from architecture review
**Files touched**:
- `src/tools/send_message.rs` — corrected side_effect from ReadOnly to External
- `src/http/handlers.rs` — list_sessions now sorted by id for stable pagination

**Tests added**: send_message_tool_is_not_readonly, list_sessions_stable_sort_by_id (80 HTTP tests total)

**Notes**:
- providers.rs expect() — already fixed; product code uses match+error! in all_presets(),
  unwrap() at lines 103/131 are inside #[cfg(test)] (acceptable per Invariant #5)
- cost.rs 0.0 vs None — cost_usd() already returns Option<f64>; unknown models already
  emit tracing::warn! at CostTracker::new(); no change needed
- MessageBus unbounded growth — multi.rs uses broadcast::channel(256) (bounded); the
  ChannelSink unbounded channel in runtime.rs is a local pipe scoped to a single turn
  and cannot accumulate across turns; no change needed
- eprintln! in main.rs/cli/ — all are startup banners or user-facing status messages;
  appropriate use of eprintln for CLI output; no change needed
