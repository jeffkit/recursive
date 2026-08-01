# Goal 353 — Mid-stream cancellation must persist the transcript (Invariant #7 fix)

**Roadmap**: Invariant hardening — #7 (finish reasons are data, not errors)

**Design principle check**:
- Implemented as: a `match` arm at the single LLM-call site in `RunCore::run_inner`.
- ❌ Does NOT add a branch *that introduces a new capability* into `run_inner` — it
  routes an existing error condition to an existing `FinishReason`, symmetric with the
  already-present `check_shutdown` (`run_core.rs:697`) → `make_outcome` path.
- No new tools, no new providers, no new deps.

## Why (root cause, with file:line)

Both streaming providers return `Err(Error::Cancelled)` when the shutdown/cancel token
fires *during* an in-flight LLM stream (between SSE chunks):

- `src/llm/anthropic.rs:346` — `return Err(Error::Cancelled);` inside `tokio::select! { _ = ct.cancelled() => ... }`
- `src/llm/openai.rs:658` — same pattern.

A comment next to each (`anthropic.rs:340`, `openai.rs:651`) claims:
> "run_core 已有逻辑翻成 FinishReason::Cancelled（Invariant #7）"

**This is false.** There is NO `Error::Cancelled` catch anywhere in the run path
(`run_core.rs`, `kernel.rs`, `runtime.rs` — verified by grep: zero matches for
`Error::Cancelled` outside the two provider files). The only `Cancelled` handling is
`FinishReason::Cancelled` at `run_core.rs:707` inside `check_shutdown`, and that runs
**only between steps** (at the top of the loop). Mid-stream cancellation escapes as `Err`.

The `Err` rides the `?` operators through:
- `dispatch_llm_step` → `call_llm(...).await?` at `run_core.rs:378`
- `run_inner` → `dispatch_llm_step(...).await?` at `run_core.rs:1284`
- `kernel.rs:346` → `core.run_inner().await?`
- `runtime.rs:664` → `self.kernel.run(ctx).await?`
- finally the catch-all `Err(e) => return Err(e)` at `runtime.rs:284`.

At that point `emit_turn_messages` (`runtime.rs:285`), `maybe_compact_cross_turn`
(`runtime.rs:307`), and the `turn_index.fetch_add` (`runtime.rs:319`) are ALL skipped.
The partial assistant text and any tool results already pushed during this turn are never
appended to the persisted transcript. **This is exactly what Invariant #7 forbids:**
> "NEVER short-circuit the transcript save; a cancelled/error run must still write its
>  partial transcript so the turn is reconstructable."

The same bug also causes `runtime.rs:326` (`if !matches!(outcome.finish_reason,
FinishReason::Cancelled)`) to never see a real Cancelled outcome from this path — so the
`SessionEnd` hook suppression logic for cancellation is dead for the mid-stream case.

## Scope (do exactly this, no more)

### 1. Catch `Error::Cancelled` at the LLM-call site in `run_inner`

At `src/run_core.rs:1284`, the current code is:

```rust
let (completion, new_final_message) = self
    .dispatch_llm_step(&specs, step, &mut total_usage)
    .await?;
```

Change it so that an `Error::Cancelled` is translated into a `FinishReason::Cancelled`
outcome via `make_outcome`, instead of bubbling as `Err`. Concretely:

```rust
let (completion, new_final_message) = match self
    .dispatch_llm_step(&specs, step, &mut total_usage)
    .await
{
    Ok(v) => v,
    Err(crate::error::Error::Cancelled) => {
        // Mid-stream cancellation: the stream was interrupted partway through
        // an LLM call. Route to the same FinishReason as check_shutdown so the
        // transcript (which already holds whatever partial messages were pushed
        // before the abort) is persisted by the caller. Invariant #7.
        let finish = FinishReason::Cancelled;
        self.emit(AgentEvent::TurnFinished {
            reason: finish_reason_str(&finish),
            steps: step,
        });
        tracing::info!(
            target: "recursive::agent",
            steps = step,
            finish = ?finish,
            "agent.run.cancelled_mid_stream"
        );
        return Ok(self.make_outcome(
            finish,
            step,
            final_message,
            total_usage,
            tool_audits,
        ));
    }
    Err(e) => return Err(e),
};
```

**Notes for the agent:**
- `finished_steps` is `step` here (NOT `step - 1` like `check_shutdown`): the
  cancellation happened *during* the current step's LLM call, so the step is
  in-progress, not pre-call. `check_shutdown` runs at loop top before the call,
  hence its `step - 1`. Mirror each path's own semantics; do not unify them.
- `final_message` may be `Some` if a prior step in the same turn already produced
  final text; pass it through unchanged.
- `tool_audits` is the accumulator already in scope at this point in `run_inner`.
- `AgentEvent::TurnFinished` + the `tracing::info!` mirror `check_shutdown`'s emission
  exactly (copy the field set), so observers/logs are consistent regardless of whether
  cancellation was detected between steps or mid-stream.
- Verify the exact `AgentEvent::TurnFinished` field names against `check_shutdown`
  (`run_core.rs:706-710`) before writing — do not guess.

### 2. Correct the misleading comments at the provider sites

- `src/llm/anthropic.rs:340` and `src/llm/openai.rs:651`: the comment asserting
  "run_core 已有逻辑翻成 FinishReason::Cancelled" becomes TRUE after step 1 — but it
  should be reworded to point at the actual catch site so the next reader can verify it.
  Change to something like:
  > "Triggered when the shutdown token fires mid-stream. Returns `Err(Error::Cancelled)`;
  >  the caller in `run_inner` translates this into `FinishReason::Cancelled` (Invariant #7)."
  (Keep it one line; the goal is a verifiable pointer, not prose.)

### 3. Tests (the headline regression test is mandatory)

Add tests in the `#[cfg(test)]` module of `src/run_core.rs` (the file already has an
extensive `TestCore`/mock-provider harness — reuse it; see existing cancellation/shutdown
tests for the pattern):

- **`cancelled_mid_stream_persists_transcript_and_finish_reason`** (HEADLINE):
  - Build a `RunCore` whose mock provider returns `Err(Error::Cancelled)` on the FIRST
    LLM call (simulate the mid-stream abort). Provide a shutdown token, but do NOT rely on
    `check_shutdown` — the error must come from the provider call itself.
  - Assert: `run_inner` returns `Ok(outcome)` with `outcome.finish_reason ==
    FinishReason::Cancelled`.
  - Assert: `outcome.steps == 0` (cancellation on step 0's LLM call).
  - Assert: the `AgentEvent::TurnFinished` event was emitted (capture via the test
    event sink the existing tests use).
  - Assert: `outcome.messages` still contains whatever the test seeded (the transcript
    is not lost).

- **`cancelled_between_steps_uses_check_shutdown_path`** (regression guard):
  - A run where the shutdown token is already cancelled at loop top (step 0, before any
    LLM call). This exercises `check_shutdown` (`run_core.rs:707`), NOT the new arm.
    Assert it still returns `Ok` with `FinishReason::Cancelled` and `steps == 0`.
    This pins the existing path so reviewers can see both arms are covered and distinct.

- **`non_cancelled_error_still_bubbles`** (negative guard):
  - A run whose mock provider returns a NON-Cancelled error (e.g. `Error::Network` /
    `Error::Provider(...)` — pick one the run path does not retry away). Assert
    `run_inner` returns `Err(...)`, proving the new `match` only swallows `Cancelled`.

Look at neighbouring tests (search the test module for `Cancelled` / `shutdown_token`)
and mirror their harness setup (`TestCore`, `MockProvider`, event capture). Do NOT
introduce a new test harness if an existing one fits.

## Files NOT to touch

- `src/runtime.rs` — no change needed; once `run_inner` returns `Ok(Cancelled)`, the
  existing `runtime.rs:283` `Ok` arm handles `emit_turn_messages` + compaction +
  `turn_index` correctly, and `runtime.rs:326`'s `SessionEnd` suppression activates.
  Do not "fix" runtime; verify by test, don't edit.
- `src/kernel.rs` — passthrough only, no edit.
- `src/llm/anthropic.rs` / `src/llm/openai.rs` — comment wording only (step 2). Do NOT
  change the `return Err(Error::Cancelled)` behaviour; the error is the correct signal,
  the *caller* must translate it.
- Anything under `crates/`, `tests/invariants/`, `.dev/flows/`.

## Acceptance

- `cargo test --workspace` green, including the 3 new tests above.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.
- The grep `rg "Error::Cancelled" src/run_core.rs src/kernel.rs src/runtime.rs` now
  returns at least one match in `src/run_core.rs` (the new catch arm) — i.e. the
  previously-dead `Cancelled` translation now exists. (The provider files still return
  the error; that's expected.)

## Notes for the agent (traps)

- **Invariant #1 (loop stays small):** this change does NOT branch `run_inner` into a
  new capability — it routes an existing error to an existing finish reason. The match
  is a 3-arm error translator at one call site, symmetric with the `?` it replaces.
  Reviewers checking invariant #1 should see this as a bug fix, not feature creep.
- **`finished_steps` semantics:** `check_shutdown` uses `step - 1` (pre-call, loop top);
  the mid-stream arm uses `step` (the call was in-flight). Do NOT unify these. If you
  are tempted to "share a helper", don't — the two values are correct for their contexts
  and a shared helper would hide the distinction. A reviewer reading this goal should be
  able to confirm the `step` vs `step - 1` choice is deliberate.
- **`make_outcome` consumes `self`:** it takes `self` by value (it owns `self.messages`).
  At the `:1284` call site `self` is still owned (we're inside `run_inner(&mut self)` →
  actually `run_inner(self)` style; confirm the exact receiver by reading the signature
  at the top of `run_inner` before writing). If the receiver is `&mut self`, you may need
  `std::mem::take` / reconstruct — but the existing `make_outcome` call sites (e.g. the
  `handle_no_tool_calls` return at ~line 1295) already work, so follow whatever pattern
  they use. Do NOT change `make_outcome`'s signature.
- **Don't touch the provider return.** The `Err(Error::Cancelled)` from the provider is
  the correct mid-layer signal. The bug is that nobody translates it; the fix is the
  translator, not silencing the signal.
- **Flaky-timing:** the tests must NOT use real `sleep` to trigger cancellation. Drive
  the mock provider to return `Err(Error::Cancelled)` directly (deterministic). The
  shutdown-token path is for `check_shutdown`; the mid-stream test simulates the
  provider-side abort explicitly.
