# Goal 285 — Fix DENIAL_LIMIT_SENTINEL Double-Push Bug

**Roadmap**: Post-Phase (Correctness) — Bug fix 1/3 from arch-review 2026-06-16

**Design principle check**:
- Implemented as: surgical fix inside `run_core.rs::execute_tool_calls` result loop
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

In `run_core.rs::run_inner`, tool results are processed in a `for` loop.
When a `DENIAL_LIMIT_SENTINEL` result is detected mid-loop, the current code:

1. Pushes earlier (non-sentinel) results via the outer loop's `push_message` at line 816
2. Then fires the nested "flush all results" inner loop — which pushes ALL results again,
   including those already pushed in step 1.

This causes duplicate `Role::Tool` messages in the transcript, breaking **Invariant #8**
(tool-call ↔ tool-result pairing must be unique). A downstream LLM call will see two
`Role::Tool` messages with the same `tool_call_id`, triggering HTTP 400 from the provider.

## Scope (do exactly this, no more)

### 1. `src/run_core.rs`

**Remove** the nested sentinel-flushing inner `for` loop inside the outer `for ToolCallOutcome in &results` loop.

**Replace** the entire sentinel-handling logic with a single pre-pass check before the outer loop:

```rust
// ── Goal-285: sentinel pre-pass — detect before any push_message calls ──────
// The old code checked for DENIAL_LIMIT_SENTINEL inside the outer loop.
// When sentinel appeared at index N > 0, results[0..N] had already been
// pushed by the outer loop's push_message; the nested inner loop then
// pushed ALL results again, duplicating results[0..N] in the transcript.
//
// Fix: scan first, flush once atomically, return immediately.
if results.iter().any(|o| o.result == DENIAL_LIMIT_SENTINEL) {
    for o in &results {
        if let Some(a) = &o.audit {
            tool_audits.insert((self.turn, o.id.clone()), a.clone());
        }
        let is_error = o.result.starts_with("ERROR: ") || o.result == DENIAL_LIMIT_SENTINEL;
        self.emit(AgentEvent::ToolResult {
            id: o.id.clone(),
            name: o.name.clone(),
            output: o.result.clone(),
            step,
            is_error,
        });
        self.push_message(Message::tool_result(o.id.clone(), o.result.clone()));
    }
    let finish = FinishReason::PermissionDenialLimit;
    self.emit(AgentEvent::TurnFinished {
        reason: finish_reason_str(&finish),
        steps: step,
    });
    return Ok(RunInnerOutcome {
        messages: self.messages,
        final_message,
        finish_reason: finish,
        total_usage,
        total_llm_latency_ms: self.total_llm_latency_ms,
        steps: step,
        tool_audits,
    });
}
```

Then **remove** the old sentinel-check block that was inside the outer for loop
(the entire `if result == DENIAL_LIMIT_SENTINEL { ... }` block at the top of the loop body).

The `stuck_window` tracking and normal `push_message` path in the outer loop
remain unchanged — they only run when no sentinel is present.

### 2. Tests

Add a test in `src/run_core.rs #[cfg(test)] mod tests` (or in the integration suite)
that verifies:

- When a batch of 2 tool calls includes a `DENIAL_LIMIT_SENTINEL` as the second result,
  the resulting transcript has **exactly 2** tool-result messages (not 3).
- The first tool result (non-sentinel) appears exactly once.
- `finish_reason == FinishReason::PermissionDenialLimit`.

You'll need `MockProvider` + a mock `PermissionHook` (or directly invoke `RunCore`
by constructing it with test fixtures). The simplest test constructs a transcript
with a pre-built assistant message with two tool calls and manually drives
`execute_tool_calls` with one real tool and one that returns the sentinel.

If wiring up a full `RunCore` is too complex, add the test as an `AgentRuntime`
integration test in `src/runtime.rs` using a `MockProvider` that returns a
`ToolCall` whose matching tool returns `Error::PermissionDeniedLimit`.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- The new regression test passes and explicitly verifies the no-duplicate-push invariant
- No existing tests regress

## Notes for the agent

- The bug is in `src/run_core.rs`, function `run_inner`, specifically the
  `for ToolCallOutcome { ... } in &results { ... }` loop starting around line 726.
- The sentinel string is `pub(crate) const DENIAL_LIMIT_SENTINEL: &str = "ERROR_DENIAL_LIMIT:"`.
- The fix must preserve: (a) audit metadata collection, (b) ToolResult event emission,
  (c) correct `push_message` ordering, (d) the final `TurnFinished` event.
- Read `src/run_core.rs` in full before editing — the context is dense.
- **DO NOT modify** `src/runtime.rs`, `src/kernel.rs`, `src/tools/mod.rs`, or any other file.
