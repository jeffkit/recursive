# Goal: anti-stuck heuristic in the agent loop

## Motivation

In a past run a weak model emitted the SAME tool call (same name, same
arguments) and got the SAME error back, then repeated this 24 times until
`max_steps` ran out. Useless work, wasted tokens. The kernel should detect
this and bail early with a distinct reason so the supervisor knows the
model is wedged (not just slow).

## Requirements

1. In `src/agent.rs`, add a new variant to `FinishReason`:

   ```rust
   Stuck { repeated_call: String, repeats: usize }
   ```

   `repeated_call` is the tool name; `repeats` is the consecutive identical
   error count that triggered the stop (always ≥ the threshold).

2. Inside `Agent::run`, track the last tool call's `(name, arguments_json,
   produced_error)` tuple. If the **same** name + arguments are emitted 3
   times in a row AND every one of those calls returned a result whose text
   starts with `"ERROR:"` (the prefix the loop already uses on tool errors),
   stop the loop and return an `AgentOutcome` with
   `finish = FinishReason::Stuck { … }`. Do NOT raise an error from
   `Agent::run` — a stuck loop is a normal terminal state, not a bug.

3. The check must use the **exact** call serialised by `serde_json` so that
   semantically-different argument objects don't accidentally collide.

4. The threshold (3) should be a `const STUCK_THRESHOLD: usize = 3;` at the
   top of `src/agent.rs`, NOT a magic number scattered in the loop.

5. Emit `StepEvent::Finished { reason: Stuck { .. }, .. }` like any other
   terminal state.

6. Add **two** unit tests in `src/agent.rs`'s existing `#[cfg(test)] mod
   tests`:
   - `stops_when_repeated_call_keeps_erroring`: MockProvider scripted to
     call a non-existent tool (`UnknownTool` -> "ERROR: ...") four times.
     Assert the outcome is `Ok` with `finish == FinishReason::Stuck { .. }`
     and `repeats == 3`.
   - `does_not_trigger_when_args_differ`: MockProvider scripted to call the
     same tool three times but with different arguments each time, all
     erroring. Assert the outcome reaches `BudgetExceeded` or a normal stop,
     NOT `Stuck`. (Set `max_steps` low enough that the test terminates.)

## Out of scope

- Don't touch `LlmProvider`, `Tool`, `ToolRegistry`, or any file outside
  `src/agent.rs`.
- Don't add new dependencies.
- Don't touch `.dev/`.
- Don't change `MAX_STEPS` defaults.

## Definition of done

- `cargo build` and `cargo test` are both green.
- New variant appears in `Debug` output (default derive on enum is fine).
- The two new tests are present and pass.
- Existing tests (especially `reports_step_budget_exceeded`) still pass —
  the anti-stuck heuristic must not interfere with the budget path.

## Final summary

When done, write a short summary listing: files touched (should be just
`src/agent.rs`), what was added (variant + check + tests), and the test
result line.
