# Goal 347 — Extract CompactionRunner to eliminate duplicate compaction logic

**Roadmap**: Post-Phase — Architecture Quality (refactoring)

**Design principle check**:
- Implemented as: new component `src/compact/runner.rs` extracted from `RunCore` and `AgentRuntime`
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- ✅ Follows Invariant #4 (new tool → new file) by analogy: new component → new file

## Why

The compaction orchestration logic is currently duplicated across three locations:

1. `RunCore::maybe_compact` (run_core.rs, ~80 lines) — intra-turn compaction
2. `AgentRuntime::maybe_compact_cross_turn` (runtime.rs, ~90 lines) — cross-turn compaction  
3. `AgentRuntime::compact_on_overflow` (runtime.rs, ~45 lines) — emergency compaction

All three follow the same pattern: circuit-breaker check → threshold check → `would_compact` pre-check → `PreCompact` hook → `apply_to_transcript` → `PostCompact` hook + event emission. The differences (microcompact prefix, event types, breaker exemption) are minor variations on the same pipeline.

This duplication is a maintenance hazard — any change to compaction orchestration must be made in three places.

## Scope (do exactly this, no more)

### 1. `src/compact/runner.rs` — new file

Create a `CompactionRunner` struct that encapsulates the compaction orchestration pipeline:

```rust
pub struct CompactionRunner {
    compactor: Option<Compactor>,
    microcompactor: Option<Microcompactor>,
    consecutive_failures: u32,
    hooks: Arc<HookRegistry>,
    event_tx: Option<mpsc::UnboundedSender<AgentEvent>>,
}
```

Public methods:

- `compact_if_needed(&mut self, transcript: &mut Vec<Message>, step: usize, last_prompt_tokens: u32, provider: &dyn ChatProvider)` — Standard path with threshold check + circuit breaker. Also runs microcompact prefix when configured. Returns `Ok(Some((removed, summary_chars)))` on success, `Ok(None)` if no compaction needed/possible, `Err` only for provider errors that should propagate.

- `compact_force(&mut self, transcript: &mut Vec<Message>, step: usize, provider: &dyn ChatProvider)` — Emergency path: no threshold check, no circuit breaker. Returns `Ok(true)` when compaction ran, `Ok(false)` when transcript was too short.

Internal pipeline (shared logic):

```
1. [compact_if_needed only] Pre-microcompact if configured → emit Microcompact event
2. [compact_if_needed only] Circuit breaker check → emit CompactionSkipped if tripped
3. [compact_if_needed only] Threshold check (should_compact) → skip if not exceeded
4. would_compact check → skip if too short
5. PreCompact hook dispatch
6. apply_to_transcript call
7a. Ok(Some) → reset breaker, PostCompact hook, emit event
7b. Ok(None) → no-op
7c. Err → [emit_only:skip] increment breaker, log, emit CompactionSkipped, return Ok(None)
```

Events emitted by the runner:
- `AgentEvent::Microcompact { step, pruned }` — only when microcompact ran
- `AgentEvent::CompactionSkipped { step, reason }` — for circuit breaker skip
- `AgentEvent::Compacted { removed, kept, summary_chars, step }` — on success (RunCore path)
- `AgentEvent::CompactionBoundary { turn, compacted_count, summary_uuid }` — on success (Runtime path)

**Make event emission configurable** via a `event_mode: CompactionEventMode` enum:
```rust
enum CompactionEventMode {
    IntraTurn,   // emits Compacted
    CrossTurn,   // emits CompactionBoundary + MessageAppended (via event_tx)
}
```

Or simpler: accept an `on_compacted: Arc<dyn Fn(usize, usize, usize, usize)>` closure.

### 2. `src/run_core.rs` — simplify

- Remove fields: `microcompactor`, `consecutive_compact_failures`
- Add field: `compaction_runner: CompactionRunner`
- Replace `maybe_compact(&mut self, step)` body with a single call to `self.compaction_runner.compact_if_needed(...)`
- Remove all compaction-related tests from run_core's test module (they move to compact/runner.rs)

### 3. `src/runtime.rs` — simplify

- Remove fields: `microcompactor`, `consecutive_compact_failures`
- Add field: `compaction_runner: CompactionRunner`
- Replace `maybe_compact_cross_turn(&mut self, ...)` body with `self.compaction_runner.compact_if_needed(...)`
- Replace `compact_on_overflow(&mut self)` body with `self.compaction_runner.compact_force(...)`
- Remove all compaction-related tests from runtime's test module (they move to compact/runner.rs)

### 4. `src/compact/mod.rs` — exports

- Add `pub mod runner;`
- Re-export `CompactionRunner` from the module

### 5. Builder wiring

- `AgentRuntimeBuilder` should construct `CompactionRunner` and pass it to `AgentRuntime`
- `AgentKernel`/`RunCore` builder should construct `CompactionRunner` for intra-turn use (with `IntraTurn` mode)
- `CompactionRunner` should accept `event_tx` and `hooks` via constructor

## Files NOT to touch

- `src/kernel.rs` — TurnContext / AgentKernel types unchanged
- `src/agent/types.rs` — FinishReason unchanged
- `src/error.rs` — Error types unchanged  
- `src/compact/mod.rs` — Compactor struct unchanged
- `src/compact/micro.rs` — Microcompactor unchanged
- `src/tools/` — all tools unchanged
- `src/llm/` — providers unchanged

## Tests to add in `src/compact/runner.rs`

- `compact_if_needed_skips_when_under_threshold`
- `compact_if_needed_skips_when_circuit_breaker_tripped`
- `compact_if_needed_resets_breaker_on_success`
- `compact_if_needed_increments_breaker_on_error`
- `compact_if_needed_runs_microcompact_before_main_check`
- `compact_force_runs_without_threshold_check`
- `compact_force_runs_without_circuit_breaker`
- `compact_force_returns_false_when_too_short`
- Event emission correctness for both `IntraTurn` and `CrossTurn` modes

Existing tests in `run_core` and `runtime` that test compaction behaviour should be moved to `runner.rs` where they test the same behaviour through `CompactionRunner`. Tests that test higher-level integration (e.g. "compaction fires during a run") stay where they are.

### Existing tests to move (from run_core tests)

- `maybe_compact_noop_when_no_compactor`
- `maybe_compact_noop_when_under_threshold`
- `maybe_compact_fires_when_over_threshold`
- `maybe_compact_fires_at_threshold_boundary`
- `maybe_compact_uses_token_threshold_intra_turn`
- `maybe_compact_kept_count_is_correct`
- `maybe_compact_circuit_breaker_skips_when_failures_exceed_threshold`
- `maybe_compact_circuit_breaker_resets_on_success`
- `maybe_compact_circuit_breaker_increments_on_error`
- `maybe_compact_circuit_breaker_accumulates_to_threshold`

### Existing tests to move (from runtime tests)

- `cross_turn_microcompact_prunes_before_summary_check`
- `cross_turn_microcompact_disabled_when_none`
- `compact_on_overflow_compacts_long_transcript`
- `compact_on_overflow_returns_false_without_compactor`
- `compact_now_invokes_compactor` (keep in runtime — it tests the public API)
- `compact_now_is_noop_without_compactor` (keep in runtime)

### Integration tests to keep in place (verify wiring, not the runner itself)

- `compact_now_invokes_compactor` — stays in runtime tests
- `compact_now_is_noop_without_compactor` — stays in runtime tests
- `context_overflow_triggers_compact_and_retry` — stays in runtime tests

## Acceptance

- `cargo test --workspace` green (all existing tests pass, moved tests pass in new location)
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- No functional change: `src/compact/mod.rs`'s `Compactor` struct is untouched
- Three call sites (`maybe_compact`, `maybe_compact_cross_turn`, `compact_on_overflow`) each reduced to single method calls
- No new `#[allow(dead_code)]` attributes introduced

## Notes for the agent

- **Read first**: `src/compact/mod.rs`, `src/compact/micro.rs` — understand the existing types
- **Read**: `src/run_core.rs` search for `maybe_compact` and `consecutive_compact_failures` — these are what you're removing
- **Read**: `src/runtime.rs` search for `maybe_compact_cross_turn`, `compact_on_overflow`, `consecutive_compact_failures` — these are what you're removing
- **The HookRegistry** in RunCore is a borrowed reference (`&'a HookRegistry`). `CompactionRunner` needs hooks too. Use `Arc<HookRegistry>` instead, or pass hooks as a parameter to `compact_if_needed`.  Check how hooks are used in run_core vs runtime — they may have different ownership patterns.
- **The event channel** in RunCore is `Option<mpsc::UnboundedSender<AgentEvent>>`. In Runtime it's an `Arc<dyn EventSink>`. The runner needs to accept both. Use `Option<mpsc::UnboundedSender<AgentEvent>>` for intra-turn events, and an optional secondary sink closure for cross-turn.
- **Microcompact** only runs in the cross-turn path (runtime), not in the intra-turn path (RunCore). The `compact_if_needed` method should accept a `run_microcompact: bool` flag or the runner should be configured with `with_microcompact: bool`.
- **DO NOT** change `Compactor`, `Microcompactor`, `ToolRegistry`, `AgentKernel`, `Message`, or any tool. Only refactor the orchestration.
- **DO NOT** change the `apply_to_transcript` signature or behavior.
