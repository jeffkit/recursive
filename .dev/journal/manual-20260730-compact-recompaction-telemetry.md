# Manual edit: compact-recompaction-telemetry

**Date**: 2026-07-30
**Goal**: 338 — Track and emit recompaction-in-chain telemetry

## Summary

Added `last_compact_turn: Option<u32>` tracking on `AgentRuntime` and three
new fields (`is_recompaction_in_chain`, `turns_since_previous_compact`,
`previous_compact_turn`) on the `CompactionBoundary` event. These distinguish
first-in-session compaction from recompaction chains (compacting again soon
after a previous compaction), enabling the TUI/journal to surface "compacted
again K turns after last compact."

## Files touched

- **`src/event.rs`** — Added 3 fields to `CompactionBoundary` variant:
  `is_recompaction_in_chain: bool`, `turns_since_previous_compact: u32`,
  `previous_compact_turn: Option<u32>`. Updated the serialization round-trip
  test fixture to include the new fields.
- **`src/runtime.rs`** — Added `last_compact_turn: Option<u32>` field to
  `AgentRuntime` (init `None`). Updated 3 methods:
  - `maybe_compact_cross_turn`: computes recompaction info from
    `self.last_compact_turn` and emits it on the `CompactionBoundary` event,
    then sets `self.last_compact_turn = Some(current_turn)`.
  - `compact_on_overflow`: same pattern (recompaction info + field update).
  - `compact_now`: sets `self.last_compact_turn` so a manual `/compact`
    followed by auto-compact is detected as a chain.
- **`tests/compact_boundary.rs`** — Added 3 new tests:
  - `first_compaction_is_not_recompaction` — verifies first compact has
    `is_recompaction_in_chain == false`, `previous_compact_turn == None`.
  - `recompaction_marks_chain_when_within_session` — drives two compactions
    in the same runtime; asserts the second has
    `is_recompaction_in_chain == true`, `previous_compact_turn == Some(0)`.
  - `manual_then_auto_detected_as_chain` — `compact_now` then cross-turn
    compact → second is `is_recompaction_in_chain == true`.
- **Did NOT touch**: `src/compact/mod.rs`, `src/run_core.rs`, `src/llm/`,
  `src/kernel.rs`, tool files, or crates outside the core library.

## Design decisions

- `last_compact_turn` on `AgentRuntime` (cross-turn owner), not `RunCore` —
  intra-turn `maybe_compact` emits the `Compacted` variant (not
  `CompactionBoundary`), so it does not need the field.
- `compact_now` only sets `last_compact_turn` without emitting a
  `CompactionBoundary` event (preserving existing behavior — `compact_now`
  emits no events).
- `turns_since_previous_compact` uses `saturating_sub` — turn index is
  monotonic within a session but a resumed session resets it; 0 is safe.

## Quality gates

- `cargo test --workspace`: all 2129 + separate integration tests green
- `cargo clippy --all-targets --all-features -- -D warnings`: clean
- `cargo fmt --all`: clean
