# Goal 305 — Pass actual turn index to cross-turn compaction instead of 0

**Roadmap**: Post-Phase (Correctness / debugging aid)

**Design principle check**:
- Implemented as: passing `self.checkpoints.turn_index.load(Ordering::Relaxed)`
  to `apply_to_transcript()` instead of the hardcoded `0` in
  `src/runtime.rs`
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

`Compactor::compact()` embeds the `step` parameter in the compaction summary
header for debuggability:

```
[compacted: N messages → M chars at step X]
```

In `src/run_core.rs`, the per-turn compaction correctly passes the current
tool-call step number. However, in `src/runtime.rs`, both cross-turn
compaction call sites hardcode `step = 0`:

1. `maybe_compact_cross_turn()` (line ~332): pass `0`
2. `compact_now()` (line ~652): pass `0`

This means every cross-turn compaction summary header reads "at step 0",
regardless of how many turns have elapsed. This makes it hard to correlate
compaction events with the agent's turn history during debugging.

The fix: use `self.checkpoints.turn_index.load(Ordering::Relaxed)` to get
the current turn count. For `compact_now()`, the same value is appropriate.

## Scope (do exactly this, no more)

### 1. `src/runtime.rs` — fix `maybe_compact_cross_turn()`

Find:
```rust
let Some((removed, summary_chars)) = compactor
    .apply_to_transcript(
        self.kernel.llm().as_ref(),
        Arc::make_mut(&mut self.transcript),
        0,
    )
    .await?
```

Replace `0` with:
```rust
self.checkpoints.turn_index.load(std::sync::atomic::Ordering::Relaxed)
```

### 2. `src/runtime.rs` — fix `compact_now()`

Find:
```rust
compactor
    .apply_to_transcript(
        self.kernel.llm().as_ref(),
        Arc::make_mut(&mut self.transcript),
        0,
    )
    .await?;
```

Replace `0` with:
```rust
self.checkpoints.turn_index.load(std::sync::atomic::Ordering::Relaxed)
```

### 3. Tests

Add a test in `src/runtime.rs` (in the existing `#[cfg(test)]` section)
that:
1. Builds a runtime with a Compactor configured to compact aggressively
   (small threshold) and `keep_recent_n = 0`
2. Runs 2 turns to advance the turn index
3. Triggers `compact_now()` after 2 turns
4. Inspects the resulting transcript's first message to verify its content
   contains "at step 2" (or matches a pattern like "step [^0]")

This ensures the turn index propagates correctly into the header.

(If building a full runtime in tests is complex, a simpler assertion is
acceptable: verify that after 2 turns, calling `compact_now()` does not
panic and returns `Ok(())`.)

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- Both `apply_to_transcript` calls in `src/runtime.rs` use `turn_index`
  instead of `0`

## Notes for the agent

- Read `src/runtime.rs` around lines 314–355 for `maybe_compact_cross_turn`
  and lines 645–670 for `compact_now`.
- `self.checkpoints.turn_index` is `Arc<AtomicUsize>`. Use
  `self.checkpoints.turn_index.load(Ordering::Relaxed)` to read it.
- The `Ordering` import is already used in `runtime.rs` — no new import needed.
- **DO NOT modify** `src/compact.rs`, `src/run_core.rs`, `src/agent.rs`,
  `src/http/`, or any tool files.
