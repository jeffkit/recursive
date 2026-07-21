# Manual edit: tui-cache-hit-snapshot

**Date**: 2026-07-20
**Goal**: Defer TUI status-bar cache-hit-rate updates to `TurnFinished` instead of recomputing on every LLM response. The live recomputation caused visible flicker on multi-step turns (tool-use loops, retries) where each `UiEvent::Usage` shifted the percentage.
**Files touched**:
- [crates/recursive-tui/src/cost.rs](file:///Users/kongjie/projects/Recursive/crates/recursive-tui/src/cost.rs)
- [crates/recursive-tui/src/ui/status.rs](file:///Users/kongjie/projects/Recursive/crates/recursive-tui/src/ui/status.rs)
- [crates/recursive-tui/src/app/event_loop.rs](file:///Users/kongjie/projects/Recursive/crates/recursive-tui/src/app/event_loop.rs)
**Tests added**:
- `cache_hit_rate_uses_snapshot_not_live_turn_counters` (status.rs) — pins the deferred-update invariant; live `turn_cache_*` counters must NOT drive display.
- `cache_hit_rate_stable_during_active_turn` (status.rs) — Usage event mid-turn must not change the bar.
- `snapshot_turn_cache_pct_computes_correct_pct` (status.rs) — direct unit test for the snapshot helper.
- `snapshot_turn_cache_pct_sets_none_when_no_data` (status.rs) — zero counters → `None` (segment hidden, no meaningless `📦0%`).
**Notes**:
- Removed `Eq` derive from `UsageStats` because the new `Option<f64>` field doesn't implement `Eq` (f64 lacks Eq). No code path depended on `Eq` for `UsageStats` (greps confirmed).
- `turn_cache_hit` / `turn_cache_miss` are still accumulated live on every Usage event and reset on `TurnStarted`; they remain the input to the snapshot.
- During the very first turn, the segment is hidden (no snapshot yet). On TurnFinished, the snapshot is taken and the segment appears with the just-completed turn's value. While the next turn runs, the figure holds steady.
- Trade-off: intra-turn cache-hit visibility is gone. If that's wanted later, add it as a separate signal (e.g. a transient overlay or a debug-only live mode) rather than re-flickering the main status bar.
- Clippy clean for my changes; pre-existing `doc list item without indentation` errors in `crates/recursive-tui/src/ui/transcript.rs` lines 282-283 are from another in-flight session and were left untouched.