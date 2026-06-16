# Goal 301 — Remove redundant inner TAIL from GoalEvaluator::evaluate

**Roadmap**: Post-Phase (Correctness / G291 follow-up)

**Design principle check**:
- Implemented as: remove the redundant `const TAIL: usize = 20` slice inside
  `src/runtime_goal.rs::GoalEvaluator::evaluate()` — the caller already
  provides a pre-sliced transcript.
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

Goal-291 made `goal_eval_transcript_tail` configurable in `AgentConfig`
(default 12). The runtime now pre-slices the transcript at the call site:

```rust
// src/runtime.rs
let tail = self.transcript_tail(self.goal_eval_transcript_tail);
let verdict = evaluator.evaluate(&condition, tail).await?;
```

However, **inside** `GoalEvaluator::evaluate()` (src/runtime_goal.rs), there
is still a leftover hardcoded:

```rust
const TAIL: usize = 20;
let tail = if transcript.len() > TAIL {
    &transcript[transcript.len() - TAIL..]
} else {
    transcript
};
```

This creates two issues:

1. **Redundant work**: The runtime already sliced to `goal_eval_transcript_tail`
   (default 12). Inside `evaluate()`, a second slice of 20 is a no-op for the
   common case (12 < 20), but it's confusing dead code.

2. **Config override when value > 20**: If a user sets
   `goal_eval_transcript_tail = 30` (to give the evaluator more context for
   complex goals), the inner `TAIL = 20` silently ignores the last 10 messages,
   defeating the configuration.

The fix is simple: remove the inner re-slicing. The `transcript` parameter
already contains only the intended messages (pre-sliced at the call site).

## Scope (do exactly this, no more)

### 1. `src/runtime_goal.rs` — remove inner TAIL from `evaluate()`

Remove these lines from `GoalEvaluator::evaluate()`:
```rust
// Only send the last 20 messages to keep the prompt cheap.
const TAIL: usize = 20;
let tail = if transcript.len() > TAIL {
    &transcript[transcript.len() - TAIL..]
} else {
    transcript
};
```

Replace all references to `tail` in the function with `transcript`.
The `transcript_text` construction should become:
```rust
let transcript_text: String = transcript
    .iter()
    // ... rest unchanged ...
```

Update the doc comment on `evaluate()` to document that the caller is
responsible for passing a pre-sliced transcript.

### 2. Tests

Add a test in `src/runtime_goal.rs` (in the existing `#[cfg(test)]` module)
that verifies a transcript with 25 messages is NOT further truncated by
`evaluate()` — i.e., all 25 messages contribute to the prompt (mock the
provider, check message content). This catches a regression to the old
TAIL=20 behavior.

Existing tests for `evaluate()` should still pass after removing the inner TAIL.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `grep "const TAIL" src/runtime_goal.rs` returns no results

## Notes for the agent

- Read `src/runtime_goal.rs` fully first — it's short (~175 lines).
- Read `src/runtime.rs` lines 808–830 to confirm the call site already
  pre-slices before passing to `evaluate()`.
- The inner `tail` variable (from the TAIL=20 slice) is used in
  `transcript_text` construction — after removing the slice, replace
  `tail.iter()` with `transcript.iter()`.
- **DO NOT modify** `src/runtime.rs`, `src/config.rs`, `src/agent.rs`,
  `src/http/`, or any other files.
