# Goal 289 — Move cross-turn compaction to after-turn, not before

**Roadmap**: Post-Phase (Arch-review cleanup) — C3 from arch-review 2026-06-16

**Design principle check**:
- Implemented as: move `maybe_compact_cross_turn` call in `runtime.rs`
  from "before LLM turn starts" to "after turn completes"
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

In `src/runtime.rs`, `chat()` calls `maybe_compact_cross_turn` immediately
after `append_user_message`, *before* `execute_kernel_turn`. This means:

```
user sends message → transcript grows → compaction fires → LLM turn runs
```

The compaction fires at the **beginning** of each turn (after user message
lands but before the LLM responds). This is the wrong time: the transcript
hasn't yet grown with the current turn's tool calls and assistant messages,
so the compaction threshold comparison is pessimistic. In the worst case,
compaction fires *every* turn even when the turn's own additions would not
push the transcript over the threshold.

The correct placement is **after** the LLM turn completes:

```
user sends message → LLM turn runs (transcript grows with assistant + tools)
  → compaction fires if total exceeds threshold
```

This way, one compaction covers the full turn's growth rather than
reactively firing at the start of each turn.

## Scope (do exactly this, no more)

### 1. `src/runtime.rs` — move `maybe_compact_cross_turn`

Read `chat()` (or equivalent user-turn entry point) in `src/runtime.rs`.
Find:
```rust
self.append_user_message(...);
self.maybe_compact_cross_turn(...).await?;    // ← currently here
let outcome = self.execute_kernel_turn(...).await?;
self.append_turn_outcome(&outcome);
```

Move the compaction call to after `append_turn_outcome`:
```rust
self.append_user_message(...);
let outcome = self.execute_kernel_turn(...).await?;
self.append_turn_outcome(&outcome);
self.maybe_compact_cross_turn(...).await?;    // ← move here
```

Verify there are no ownership / borrow issues with this rearrangement
(the borrow checker will tell you if there are, and the fix is usually
wrapping with a block or cloning a small value).

### 2. Tests

Existing tests should pass. No new tests needed unless you can write a
simple unit test that verifies compaction fires after (not before) the turn.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `maybe_compact_cross_turn` is called *after* `append_turn_outcome` in `chat()`

## Notes for the agent

- Read `src/runtime.rs` `chat()` method and `maybe_compact_cross_turn()` first.
- The move may be a 2-line change. Don't over-complicate it.
- Check for borrow/lifetime errors after the move; the fix is usually minor.
- **DO NOT modify** `src/agent.rs`, `src/run_core.rs`, `src/kernel.rs`.
- **DO NOT call `exit_plan_mode` or `request_plan_mode`.** Headless run.
