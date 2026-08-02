# Goal 363 — `recursive loop` must honor exit codes; broaden `exit_for_finish` coverage

**Roadmap**: CLI correctness — exit-code contract for scripting

**Design principle check**:
- Implemented as: (1) one-line fix in `run_loop` to return `exit_for_finish`'s result
  instead of discarding it; (2) broadening `exit_for_finish`'s match arms so non-success
  finish reasons exit non-zero; (3) tests.
- ❌ Does NOT touch the agent kernel, run loop, tools, or any invariant. The runtime already
  produces correct `FinishReason`; this goal only changes how the CLI translates it to a
  process exit code.
- No new deps.

## Why (two distinct bugs, with evidence)

### Bug 1 — `run_loop` discards the exit code; `recursive loop` ALWAYS exits 0

`crates/recursive-cli/src/main.rs:2030-2033`:
```rust
if let Some(last) = outcomes.last() {
    let _ = cli::output::exit_for_finish(&last.finish_reason, last.steps);
}
Ok(())
```
`exit_for_finish` returns `anyhow::Result<()>` — `bail!` on `BudgetExceeded`, `Ok(())`
otherwise. Binding it to `let _` throws the result away, and the trailing `Ok(())` means
the process exits 0 regardless. A supervising script running `recursive loop` cannot tell
that the agent burned its budget — it looks successful.

Compare `run_once` (`main.rs:2419`) and `cmd_resume` (`cli/resume.rs:639`), which both
correctly `return exit_for_finish(...)` / `?` it. `run_loop` is the outlier.

### Bug 2 — `exit_for_finish` only treats `BudgetExceeded` as non-zero; everything else is exit 0

`crates/recursive-cli/src/cli/output.rs:125-130`:
```rust
pub(crate) fn exit_for_finish(finish: &FinishReason, steps: usize) -> anyhow::Result<()> {
    match finish {
        FinishReason::BudgetExceeded => {
            anyhow::bail!("agent exceeded step budget ({steps})")
        }
        _ => Ok(()),
    }
}
```
The `_ => Ok(())` catch-all means `Stuck`, `TranscriptLimit`, `PermissionDenialLimit`,
`WallClockExceeded`, and `ProviderStop("404")` (a provider crash) all exit 0 —
indistinguishable from a clean `NoMoreToolCalls` success. (`Cancelled` is intentionally
`Ok(())` per the doc comment at output.rs:118-123 — user-initiated shutdown should not
auto-resume; keep that.)

Effect: a wrapper script supervising `recursive` cannot distinguish "agent finished
normally" from "agent got stuck in a loop" / "hit the wall-clock deadline" / "provider
crashed". All look like success.

## Scope (do exactly this, no more)

### 1. Fix `run_loop` to honor the exit code

`crates/recursive-cli/src/main.rs:2030-2033` — change `let _ = ...; Ok(())` to `return ...`:

```rust
if let Some(last) = outcomes.last() {
    return cli::output::exit_for_finish(&last.finish_reason, last.steps);
}
Ok(())
```
This mirrors `run_once` at `main.rs:2419`. Confirm the enclosing function's return type is
`anyhow::Result<()>` (it is — `run_loop` returns `Result` and `main` `?`s it).

### 2. Broaden `exit_for_finish` to cover the failure finish reasons

`crates/recursive-cli/src/cli/output.rs:125-130` — add explicit arms for each non-success,
non-cancel finish reason. Each should `bail!` with a descriptive message (the message goes
to stderr via anyhow and sets exit code 1). `NoMoreToolCalls` stays `Ok(())` (true success).
`Cancelled` stays `Ok(())` (intentional — see the existing doc comment; do NOT change this).

```rust
pub(crate) fn exit_for_finish(finish: &FinishReason, steps: usize) -> anyhow::Result<()> {
    match finish {
        FinishReason::NoMoreToolCalls => Ok(()),
        FinishReason::Cancelled => Ok(()),  // user-initiated; doc'd at output.rs:118-123
        FinishReason::BudgetExceeded => {
            anyhow::bail!("agent exceeded step budget ({steps})")
        }
        FinishReason::WallClockExceeded { secs } => {
            anyhow::bail!("agent exceeded wall-clock timeout ({secs}s)")
        }
        FinishReason::Stuck { repeated_call, repeats } => {
            anyhow::bail!("agent stuck: repeated tool call '{repeated_call}' ({repeats}x)")
        }
        FinishReason::TranscriptLimit { chars, limit } => {
            anyhow::bail!("transcript size {chars} exceeded hard limit {limit} and could not be reduced")
        }
        FinishReason::PermissionDenialLimit => {
            anyhow::bail!("agent hit permission denial limit (loop of denied tool calls)")
        }
        FinishReason::ProviderStop(reason) => {
            // A provider stop with a non-success reason string (e.g. "rate_limited",
            // "404", "context_length_exceeded") is a failure. The empty-string case
            // is treated as success (some providers send a bare stop with no reason).
            if reason.is_empty() {
                Ok(())
            } else {
                anyhow::bail!("provider stopped: {reason}")
            }
        }
    }
}
```
**Read `FinishReason`'s exact variants in `src/agent/types.rs` before writing** — the
review cited them but verify field names (`Stuck { repeated_call, repeats }`,
`TranscriptLimit { chars, limit }`, `WallClockExceeded { secs }`). Use the real field names.

Note the `ProviderStop` design decision: a bare `ProviderStop("stop")` (the normal
OpenAI/Anthropic stop) is a successful completion — only non-empty / non-`"stop"` reason
strings indicate failure. Read how `ProviderStop` is constructed in `run_core.rs` to confirm
the success case uses `"stop"`; if so, treat `reason == "stop"` as success too (mirror
whatever the existing semantics are — do NOT accidentally exit-1 a normal stop).

### 3. Tests

Add tests in `crates/recursive-cli/src/cli/output.rs`'s test module (or create one if it
doesn't exist — the review found no `exit_for_finish` tests today):

- `exit_for_finish_success_returns_ok` — `NoMoreToolCalls` → `Ok(())`.
- `exit_for_finish_cancelled_returns_ok` — `Cancelled` → `Ok(())` (pins the intentional
  semantics; prevents a future "fix" from breaking the self-improve auto-resume contract).
- `exit_for_finish_budget_exceeded_errors` — `BudgetExceeded` → `Err`.
- `exit_for_finish_stuck_errors` — `Stuck { repeated_call: "Read".into(), repeats: 3 }` →
  `Err` (assert the message contains "stuck").
- `exit_for_finish_wallclock_errors` — `WallClockExceeded { secs: 600 }` → `Err`.
- `exit_for_finish_transcript_limit_errors` — `TranscriptLimit { chars: 100000, limit: 80000 }`
  → `Err`.
- `exit_for_finish_provider_stop_stop_is_ok` — `ProviderStop("stop".into())` → `Ok(())`
  (normal completion).
- `exit_for_finish_provider_stop_error_fails` — `ProviderStop("rate_limited".into())` →
  `Err`.

These are pure unit tests on `exit_for_finish` (no runtime needed). They pin the exit-code
contract so the `run_loop` fix + the broadened arms never silently regress.

If there is no existing test module in `output.rs`, add `#[cfg(test)] mod tests { ... }`
at the bottom and `use super::*; use crate::agent::FinishReason;` (verify the import path —
it may be `recursive::agent::FinishReason`).

## Files NOT to touch

- `src/run_core.rs`, `src/kernel.rs`, `src/runtime.rs`, `src/agent/types.rs` — the
  `FinishReason` enum is correct; this goal only consumes it.
- `run_once` / `cmd_resume` paths — they already `?`/`return` correctly; don't refactor them.
- The `Cancelled`-is-`Ok(())` semantics — that's an intentional contract (doc'd at
  output.rs:118-123) for the self-improve auto-resume flow. Do NOT change it.
- HTTP / AG-UI handlers — separate code path.
- `.dev/flows/`.

## Acceptance

- `cargo test -p recursive-cli output::` — the new exit-code tests pass.
- `cargo test --workspace` green overall.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.
- Grep: `rg "let _ = .*exit_for_finish" crates/recursive-cli/` returns **0 hits** (the
  `run_loop` discard is gone).
- `exit_for_finish` has an explicit arm for every `FinishReason` variant (no `_ =>`
  catch-all) — grep-verifiable: the match in `output.rs` covers all 8 variants by name.

## Notes for the agent (traps)

- **`ProviderStop` success vs failure.** A normal LLM completion produces
  `ProviderStop("stop")` (OpenAI) or the Anthropic equivalent. Read `run_core.rs`'s
  `handle_no_tool_calls` / wherever `ProviderStop` is constructed to see what string means
  "normal stop". Treat that (and empty string) as `Ok`; everything else as `Err`. Do NOT
  blindly `Err` on all `ProviderStop` — that would make every successful run exit 1.
- **`Cancelled` MUST stay `Ok(())`.** The self-improve flow watches the CLI exit code: a
  non-zero exit triggers auto-resume of the next goal, which would re-run something the user
  explicitly SIGINT'd. The doc comment at output.rs:118-123 explains this. Preserve it.
- **`run_loop` return type.** Confirm `run_loop` returns `anyhow::Result<()>` (or compatible)
  so `return exit_for_finish(...)` type-checks. If it returns a different error type, use
  `?` with a `.map_err` instead — but it's almost certainly `anyhow::Result`.
- **Message content for `bail!`.** Include the relevant field values (`steps`, `secs`,
  `repeated_call`) in the message — these go to stderr and help the user diagnose. Match the
  existing `BudgetExceeded` message style.
- **Don't change exit code NUMBERS.** This goal keeps the binary `Ok(())` → 0 / `Err` → 1
  (via anyhow's main handler) contract. Introducing distinct codes (2 for config, 130 for
  SIGINT) is a separate, larger UX change — out of scope. Just ensure failures exit 1, not 0.
- **Test the function, not the process.** Testing actual process exit codes requires
  spawning the binary; that's heavy. Test `exit_for_finish`'s `Result` directly — that's the
  contract. The `run_loop` one-liner fix is correct-by-construction (mirrors `run_once`).
