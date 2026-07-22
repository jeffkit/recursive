# Goal 336 — Emit cache-hit/miss metrics on the compaction boundary event

**Roadmap**: Compaction upgrade (WS-5a — observe before optimizing)

**Design principle check**:
- Implemented as: add `cache_hit_tokens` / `cache_miss_tokens` fields to the
  `CompactionBoundary` `AgentEvent`, populated from the just-completed turn's
  `TokenUsage`.
- ❌ Does NOT branch inside `src/run_core.rs::RunCore::run_inner`.
- ❌ Does NOT change compaction behavior — pure telemetry.

## Why

The compaction upgrade plan's WS-5b (cache-preserving compaction, goal 341)
is architecturally uncertain: it depends on whether providers accept a
mid-transcript summary that keeps the recent prefix cache intact. Before
spending that work, we need data: how much prompt-cache hit rate does a
single LLM-summary compaction actually destroy today?

Recursive already normalizes `cache_hit_tokens` / `cache_miss_tokens` in
`TokenUsage` (`src/llm/chat.rs:33`), and the cross-turn path has the
turn's `usage` (`runtime.rs:305` passes `turn_outcome.usage.prompt_tokens`).
This goal threads `cache_hit_tokens` / `cache_miss_tokens` onto the
`CompactionBoundary` event so the journal / TUI / HTTP sink can record the
pre-compact cache state. Post-compact cache is observable on the *next*
turn's `Usage` event — no new field needed for that side.

This is "measure first": the data from this goal decides whether goal 341
is worth doing and what shape it should take.

## Scope (do exactly this, no more)

### 1. `src/event.rs` — extend `CompactionBoundary`

Find the existing `CompactionBoundary { turn, compacted_count, summary_uuid }`
variant (grep `CompactionBoundary`). Add two fields:
```rust
CompactionBoundary {
    turn: u32,
    compacted_count: usize,
    summary_uuid: Option<Uuid>,
    cache_hit_tokens: u32,   // NEW
    cache_miss_tokens: u32,  // NEW
},
```
Update every construction site and every `match` arm. Grep
`AgentEvent::CompactionBoundary` and `CompactionBoundary {` to find them.
The TUI sink, HTTP sink, journal writer, and tests all match on `AgentEvent`
exhaustively — add the fields with sensible defaults (`0`) at sites that
don't have the data (e.g. intra-turn `Compacted` is a different variant;
only `CompactionBoundary` changes here).

### 2. `src/runtime.rs` — populate the fields

In `maybe_compact_cross_turn`, the turn's `TokenUsage` is available as
`last_prompt_tokens`'s sibling — but `maybe_compact_cross_turn` currently
only receives `last_prompt_tokens: u32`. Change the signature to receive
the full `last_usage: TokenUsage` (or add `cache_hit_tokens`/`cache_miss_tokens`
params) from the call site at `runtime.rs:305`:
```rust
self.maybe_compact_cross_turn(turn_outcome.usage).await?;
```
and update the method:
```rust
async fn maybe_compact_cross_turn(&mut self, last_usage: TokenUsage) -> Result<()> {
    // ... should_compact uses last_usage.prompt_tokens ...
    // ... on compaction, emit:
    self.event_sink.emit(AgentEvent::CompactionBoundary {
        turn: ...,
        compacted_count: removed,
        summary_uuid: None,
        cache_hit_tokens: last_usage.cache_hit_tokens,
        cache_miss_tokens: last_usage.cache_miss_tokens,
    }).await;
}
```
Do the same for `compact_on_overflow` — but that path has no usage reading
(the turn failed before reporting usage); pass `0`/`0` there and note it in
the journal. `compact_now` (manual) likewise passes `0`/`0`.

### 3. Journal / sink recording

Wherever the journal or TUI already renders `CompactionBoundary` (grep the
TUI transcript renderer and the journal writer for `CompactionBoundary` /
`compacted_count`), append the cache fields to the rendered line, e.g.:
```
⊕ Conversation compacted: N → 1 (S chars) [cache hit H / miss M]
```
This is a display-only change.

### 4. Tests

- `compaction_boundary_emits_cache_metrics` — drive a compaction with a
  `MockProvider` that reports `cache_hit_tokens`/`cache_miss_tokens` in its
  `Completion.usage`; assert the emitted `CompactionBoundary` carries those
  values.
- `compaction_boundary_cache_zero_when_no_usage` — provider reports no usage
  → fields are `0` (no panic, invariant #5).
- All existing `CompactionBoundary` match arms still compile (exhaustiveness).

## Acceptance

- `cargo test --workspace` green; clippy clean; fmt clean.
- `CompactionBoundary` carries `cache_hit_tokens` / `cache_miss_tokens`;
  every match arm compiles.
- No behavior change to compaction decisions (this goal is telemetry only).

## Notes for the agent

- This goal is a **prerequisite for goal 341** (cache-preserving
  compaction) only in the sense of providing decision data — 341 does not
  *compile*-depend on it. Land 336 first, run a few self-improve long
  sessions, read the journal cache numbers, then decide 341's shape.
- Do NOT change `should_compact` or any threshold in this goal.
- `TokenUsage` is `Copy`/`Clone` — passing it by value is cheap.
- **DO NOT modify** `src/compact/mod.rs` logic, `src/run_core.rs` intra-turn
  path (it emits `Compacted`, a different variant — leave it), `src/llm/`,
  `src/kernel.rs`, or tool files.
- Journal entry: `.dev/journal/manual-<YYYYMMDD>-compact-cache-telemetry.md`,
  including the first observed cache hit/miss numbers around a compaction.
