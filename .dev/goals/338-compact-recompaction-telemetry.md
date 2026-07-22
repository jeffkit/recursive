# Goal 338 — Track and emit recompaction-in-chain telemetry

**Roadmap**: Compaction upgrade (WS-6a — observe pathological re-compaction)

**Design principle check**:
- Implemented as: a `last_compact_turn` field on `AgentRuntime` + three new
  fields on `CompactionBoundary` (`is_recompaction_in_chain`,
  `turns_since_previous_compact`, `previous_compact_turn`).
- ❌ Does NOT branch inside `src/run_core.rs::RunCore::run_inner`.
- ❌ Does NOT change compaction behavior — pure telemetry.

## Why

A pathological session compacts, then on the very next turn is still over
the threshold and compacts again — a "recompaction chain." Each compaction
is an expensive LLM call that loses more context, and a chain signals the
threshold is too low, the summary is too verbose, or the session is
genuinely unbounded. Without telemetry these chains are invisible.

fake-cc tracks this with `RecompactionInfo { is_recompaction_in_chain,
turns_since_previous_compact, previous_compact_turn_id, auto_compact_threshold }`
and emits it on the `tengu_compact` event, distinguishing same-chain loops
(H2) from cross-agent (H1/H5) and manual (H3). Recursive has no multi-agent
compaction, so the distinction collapses to "did we compact again within N
turns of the last compact?" — but the signal is the same: a chain means the
compaction strategy is failing for this session.

This goal adds the tracking so the journal/TUI can surface "compacted again
K turns after the last compaction" and the data can drive future threshold
tuning (and the free-text prompt upgrade in goal 339).

## Scope (do exactly this, no more)

### 1. `src/runtime.rs` — track last compact turn

- Add field `last_compact_turn: Option<u32>` to `AgentRuntime`, init `None`.
- In `maybe_compact_cross_turn`, after a successful compaction (the
  `Some((removed, summary_chars))` branch), compute the recompaction info
  BEFORE resetting:
  ```rust
  let current_turn = self.checkpoints.turn_index.load(Ordering::Relaxed) as u32;
  let is_recompaction = self.last_compact_turn.is_some();
  let turns_since = match self.last_compact_turn {
      Some(prev) => current_turn.saturating_sub(prev),
      None => 0,
  };
  let previous_compact_turn = self.last_compact_turn;
  // ... emit CompactionBoundary with the new fields ...
  self.last_compact_turn = Some(current_turn);
  ```
- Do the same in `compact_on_overflow` and `compact_now` (they also set
  `last_compact_turn` so a manual `/compact` followed by an auto-compact is
  still detected as a chain). Keep the manual path's `is_recompaction` logic
  identical.

### 2. `src/event.rs` — extend `CompactionBoundary`

Add (alongside the cache fields from goal 336):
```rust
CompactionBoundary {
    turn: u32,
    compacted_count: usize,
    summary_uuid: Option<Uuid>,
    cache_hit_tokens: u32,
    cache_miss_tokens: u32,
    is_recompaction_in_chain: bool,        // NEW
    turns_since_previous_compact: u32,     // NEW
    previous_compact_turn: Option<u32>,    // NEW
},
```
Update all construction sites and match arms (grep `CompactionBoundary`).
Sites without the data (e.g. tests) pass `false`/`0`/`None`.

### 3. Display

Wherever the TUI / journal renders `CompactionBoundary` (from goal 336),
append the chain marker when `is_recompaction_in_chain`:
```
⊕ Conversation compacted: N → 1 (S chars) [cache H/M] [re-compact, +K turns since last]
```

### 4. Tests

- `recompaction_marks_chain_when_within_session` — drive two compactions in
  the same runtime; assert the second `CompactionBoundary` has
  `is_recompaction_in_chain == true`, `turns_since_previous_compact == <delta>`,
  `previous_compact_turn == <first turn>`.
- `first_compaction_is_not_recompaction` — first compact has
  `is_recompaction_in_chain == false`, `previous_compact_turn == None`.
- `manual_then_auto_detected_as_chain` — `compact_now` then a cross-turn
  compact → second is `is_recompaction_in_chain == true`.
- All `CompactionBoundary` match arms compile.

## Acceptance

- `cargo test --workspace` green; clippy clean; fmt clean.
- `CompactionBoundary` carries the three new fields; all match arms compile.
- No compaction-decision behavior change (telemetry only).

## Notes for the agent

- `last_compact_turn` lives on `AgentRuntime` (cross-turn owner), not
  `RunCore` — intra-turn `maybe_compact` emits the `Compacted` variant (not
  `CompactionBoundary`), so it does not need the field. If a future goal
  wants intra-turn chain tracking, add a parallel field to `RunCore`; not
  needed now.
- `turns_since_previous_compact` uses `saturating_sub` — turn index is
  monotonic within a session but a resumed session resets it; `0` is a safe
  floor.
- This goal composes with goal 336 (both add fields to the same variant).
  Land 336 first, then 338; both are additive and independent in logic.
- **DO NOT modify** `src/compact/mod.rs` logic, `src/run_core.rs`,
  `src/llm/`, `src/kernel.rs`, or tool files.
- Journal entry: `.dev/journal/manual-<YYYYMMDD>-compact-recompaction-telemetry.md`,
  noting any recompaction chains observed in the first dogfood sessions.
