# Goal 137 — Graceful shutdown: wire CancellationToken end-to-end

**Roadmap**: Phase 17.4 — Graceful shutdown + in-flight request draining
(completion of g120, which landed signal handling but never connected
the token to the agent loop)

**Design principle check**:
- Implemented as: a new `Cancelled` termination check at the top of
  `RunCore::run_inner`'s step loop, parallel to the existing
  `BudgetExceeded` and `TranscriptLimit` checks. The token is plumbed
  from `main.rs::shutdown_signal` → `AgentRuntime` builder →
  `AgentKernel` → `RunCore`.
- `FinishReason::Cancelled` is **already declared** in src/agent.rs:198
  (added by g120) — this goal makes it actually emittable.
- ⚠️ Touches `agent.rs`'s main loop (a single termination check at the
  top of the step body). The user explicitly approved this as part of
  triggering the OPERATIONS.md §6 HITL gate. Approved scope: add ONE
  cancellation check at the loop head, in the same pattern as the
  existing `BudgetExceeded` / `TranscriptLimit` checks. Nothing else
  in `Agent::run` may change.
- ❌ Does NOT introduce new "capabilities" inside the loop —
  cancellation is a termination condition, semantically aligned with
  `BudgetExceeded` (also data, not error; invariant #7).
- ❌ Does NOT add any new dependency.

## Why

Goal 120 (commit 64dfec1, "graceful-shutdown — runtime integration
deferred") landed the signal handling and introduced
`FinishReason::Cancelled`, but the `shutdown` token in main.rs was
only used to print a message **after** the agent run finished. The
token was never plumbed into `AgentRuntime`, `AgentKernel`, or
`RunCore`. The agent's own `shutdown_token` field
(src/agent.rs:253) and `set_shutdown_token` setter (src/agent.rs:870)
are also dead — never wired up.

Result: `recursive run "long task"` ignores Ctrl-C until the LLM
call finishes naturally. `FinishReason::Cancelled` is dead code
(grep confirms it's never constructed anywhere in src/).

This goal closes that gap.

The HTTP server's `axum::serve(...).with_graceful_shutdown(...)`
(src/main.rs:386) already works correctly — axum's mechanism
operates at the listener level, independent of agent state. This
goal does **not** change that path; it only fixes the per-run agent
path.

## Scope (do exactly this, no more)

### 1. Plumb token through the kernel/runtime layers

Add `shutdown_token: Option<CancellationToken>` to:

- **`AgentKernel`** (src/kernel.rs:145, `pub(crate)` field) and its
  builder (`AgentKernelBuilder` around src/kernel.rs:286). Builder
  method:

  ```rust
  pub fn shutdown_token(mut self, token: CancellationToken) -> Self {
      self.shutdown_token = Some(token);
      self
  }
  ```

  In `AgentKernel::with_tools`, propagate the token to the cloned kernel
  (so multi-agent sub-agents inherit shutdown semantics).

- **`AgentRuntime`** (src/runtime.rs:80) and `AgentRuntimeBuilder`
  (src/runtime.rs:337). Builder method same shape as above. The
  runtime forwards the token to the kernel at `build()` time.

- **`RunCore`** (src/agent.rs:273). New field
  `pub(crate) shutdown_token: Option<CancellationToken>`. The kernel
  populates it when constructing `RunCore` (src/kernel.rs:234-249).

The legacy `Agent` (deprecated) already has the field; just make
`Agent::run`'s `RunCore` construction (src/agent.rs:1041-1057)
forward it.

### 2. Add the termination check at the top of the step loop

In `RunCore::run_inner` (src/agent.rs:545), at the top of the
`for step in 1..=self.max_steps` body, **before** the transcript
budget check at line 559, add:

```rust
// ---- shutdown cancellation -------------------------------------------
if let Some(ref token) = self.shutdown_token {
    if token.is_cancelled() {
        let finish = FinishReason::Cancelled;
        self.emit(StepEvent::Finished {
            reason: finish.clone(),
            steps: step.saturating_sub(1),
        });
        tracing::info!(
            target: "recursive::agent",
            steps = step.saturating_sub(1),
            tokens_in = total_usage.prompt_tokens,
            tokens_out = total_usage.completion_tokens,
            finish = ?finish,
            llm_latency_ms = self.total_llm_latency_ms,
            "agent.run.complete"
        );
        return Ok(RunInnerOutcome {
            messages: self.messages,
            final_message,
            finish_reason: finish,
            total_usage,
            total_llm_latency_ms: self.total_llm_latency_ms,
            steps: step.saturating_sub(1),
            plan_buffer: self.plan_buffer,
            plan_confirmed: self.plan_confirmed,
        });
    }
}
```

`step.saturating_sub(1)` because if cancellation is observed at the
top of step N, the agent has completed N-1 full steps (the in-flight
LLM call from step N-1 already finished and was processed; we never
started step N's LLM call).

The pattern mirrors the existing `BudgetExceeded` and
`TranscriptLimit` blocks — same shape (`emit StepEvent::Finished`,
log, return `RunInnerOutcome`). Termination check, not new capability.

### 3. Wire main.rs to actually pass the token

In `build_runtime` (src/main.rs:979), accept an
`Option<CancellationToken>` parameter and forward it to the
`AgentRuntimeBuilder`. All call sites that currently invoke
`build_runtime(...)` must be updated:

- `run_once` (src/main.rs:1548) — passes `Some(shutdown.clone())`.
- `run_resumed` — same.
- HTTP server's `build_runtime` invocations inside session handlers
  (src/http.rs run_agent / send_session_message) pass `None` (the HTTP
  layer relies on axum's `with_graceful_shutdown`, not per-run token
  propagation; an HTTP client can also drop the connection to signal
  abandon — out of scope for this goal).
- `multi.rs` and any other build sites pass `None`.

### 4. CLI exit semantics

In `exit_for_finish` (src/main.rs:1253), do **not** make `Cancelled`
return `Err`. The current fall-through `_ => Ok(())` is correct —
shutdown is user-initiated, not an error. Document this with a
comment so future readers don't "fix" it.

When `outcome.finish_reason == FinishReason::Cancelled`:
- `eprintln!` a clean message: `"shutdown: agent stopped at step N"`.
- Save the session as before (the existing `finalize_session_writer`
  / `finalize_cost_tracker` paths already run after `agent.run()`
  returns — no change needed there).

The existing `if shutdown.is_cancelled()` post-run print at
src/main.rs:1658 becomes redundant once the agent itself emits
`FinishReason::Cancelled`. Either remove that block or keep it as a
belt-and-suspenders safety net (preference: remove — duplicated
output is ugly).

### 5. Tests

In `tests/integration.rs` (NOT tests/http.rs — this is agent-loop
behavior, not HTTP API):

- **Test A — `cancellation_before_first_step_returns_cancelled_at_step_zero`**:
  Build a runtime with a token that's already cancelled. Call
  `runtime.run("anything")`. Assert
  `outcome.finish_reason == FinishReason::Cancelled` and
  `outcome.steps == 0`.

- **Test B — `cancellation_during_run_terminates_loop`**: Build a
  runtime with a `MockProvider` scripted to return 3+ tool calls
  (not final). Spawn a task that cancels the token after the first
  message is observed. Assert the final outcome is
  `FinishReason::Cancelled` with `steps < scripted_total`. Use a
  `tokio::sync::Notify` or atomic flag inside the mock provider to
  signal "first turn started, safe to cancel now" — avoid wall-clock
  sleeps.

- **Test C — `no_token_means_no_cancellation_check`**: Build a runtime
  without a token (`AgentRuntimeBuilder` default). Run a normal
  scripted MockProvider scenario; outcome is `NoMoreToolCalls`. This
  guards against accidentally making the cancellation check
  unconditional.

- **Test D — `kernel_with_tools_propagates_shutdown_token`**: Pure unit
  test: build a kernel with token; clone via `with_tools`; assert the
  cloned kernel still has the token. (Multi-agent sub-agent semantics.)

- **Test E — `cancelled_does_not_dispatch_session_end_hook`**: g120
  already established that hook dispatch is gated on
  `NoMoreToolCalls | Stuck | BudgetExceeded` (src/agent.rs:1077-1085).
  `Cancelled` is NOT in that list — that is intentional. Assert: a
  hook counting `SessionEnd` events sees zero invocations after a
  cancelled run.

(g120's spec proposed Tests A and B; this goal adds C, D, E to cover
the wiring gaps that allowed the original integration to be deferred
in the first place.)

## Acceptance

- `cargo build --features http` green.
- `cargo test --all-features` green; the 5 new tests pass.
- `cargo fmt --all -- --check` clean.
- `cargo clippy --all-targets --all-features -- -D warnings` clean.
- Backward compatibility: `recursive run "..."` without sending any
  signal behaves identically — finish reason is `NoMoreToolCalls`,
  exit 0.
- `FinishReason::Cancelled` is now reachable: a `grep` of `src/`
  finds at least one production-code constructor.
- Files modified:
  - `src/agent.rs` (~25 lines: termination check + RunCore field +
    Agent::run forwarding; existing dead `shutdown_token` field/setter
    on `Agent` stay as-is — they are part of the deprecated path)
  - `src/kernel.rs` (~30 lines: AgentKernel field, builder method,
    `with_tools` propagation, `RunCore` construction in `run`)
  - `src/runtime.rs` (~25 lines: AgentRuntime field, builder method,
    forwarding to kernel)
  - `src/main.rs` (~15 lines: `build_runtime` signature, callers,
    `exit_for_finish` Cancelled comment, run_once cleanup)
  - `src/http.rs` (~3 lines: HTTP-side `build_runtime` callers pass
    `None`)
  - `src/multi.rs` if it has `build_runtime`-equivalent calls (~5 lines)
  - `tests/integration.rs` (~120 lines: 5 new tests)
- No new dependency in `Cargo.toml`.

## Notes for the agent

- `tokio_util::sync::CancellationToken` is already a `tokio_util` dep;
  the agent already uses it (src/agent.rs:17). No new dep.
- `CancellationToken` is `Clone` (Arc'd internally). Pass by value
  in builders.
- The `Agent` struct (deprecated) already has `shutdown_token` and
  `set_shutdown_token` (src/agent.rs:253, 870). DO NOT remove them
  — they're public API and removing them is a breaking change. Just
  forward to `RunCore` in `Agent::run` (src/agent.rs:1041 RunCore
  construction).
- The check goes in `RunCore::run_inner` (src/agent.rs:545), NOT in
  the `Agent::run` wrapper. The kernel-based runtime path goes
  directly through `AgentKernel::run` → `RunCore::run_inner`,
  bypassing `Agent::run`.
- Exit code: `Cancelled` is exit 0. This is **intentional** —
  user-initiated shutdown is not an error. self-improve.sh's
  auto-resume gate (which keys on non-zero exit + `BudgetExceeded`)
  must NOT trigger on cancellation.
- Hook dispatch: `Cancelled` should NOT trigger `HookEvent::SessionEnd`
  — same as the existing `PlanPending` / `ProviderStop` cases at
  src/agent.rs:1077-1085. Test E enforces this.
- Test B's "race" against cancellation is the trickiest part. The
  cleanest pattern: have `MockProvider` accept an optional
  `Arc<tokio::sync::Notify>` that it `notify_one()`s after returning
  the first completion. The test awaits the notify, then cancels the
  token. This is deterministic and doesn't rely on sleep.
- DO NOT modify the agent's main loop body except for the single
  termination check at its top. DO NOT add cancellation checks
  inside tool execution, inside compaction, inside plan handling,
  etc. The goal scope is "between steps", not "during a step".
- DO NOT change the existing `BudgetExceeded` / `TranscriptLimit`
  blocks while you're nearby. Out of scope.
