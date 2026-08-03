# Goal 383 — TUI graceful cancel via CancellationToken

**Roadmap**: TUI — durability + correctness. When the user hits Ctrl+C (or
the app is backgrounded hard) mid-reply, the TUI hard-kills the turn task
with `JoinHandle::abort()`. That bypasses the kernel's cancellation path
entirely, so `make_cancelled_outcome` never runs and nothing from the
interrupted turn reaches disk. It then calls
`truncate_transcript(pre_turn_len)`, which rolls the *in-memory* transcript
back while leaving the on-disk JSONL untouched — creating a silent
memory/disk split. Both gaps vanish once the TUI uses the same
`CancellationToken` mechanism the HTTP layer already uses.

**Design principle check**:
- Implemented as: swap `handle.abort()` for `token.cancel()` + natural
  await in the TUI turn driver, mirroring `src/http/handlers.rs:908-915`;
  drop the memory-only `truncate_transcript(pre_turn_len)` rollback on the
  aborted path so memory stops diverging from disk. No new capability, no
  new finish reason.
- ❌ Does NOT touch `src/run_core.rs::run_inner` — the cancellation routing
  there (`Error::Cancelled` arm + `make_cancelled_outcome`) already exists
  from Goal 382 / Invariant #7 wiring; this goal only changes the TUI
  *caller* so that routing actually gets exercised. Invariant #1 untouched.
- Depends on Goal 382 for the "stream read error returns partial content"
  half, but is independently valuable: even before 382, graceful cancel
  lets a user-initiated Ctrl+C persist whatever the kernel had finished
  (completed tool results, earlier assistant text in a multi-step turn)
  instead of throwing it all away.
- ❌ Tests-only-on-TUI gate: this goal touches `crates/recursive-tui/src/`,
  so `tui-test-presence.sh` (hard gate) + `tui-mutants.sh` apply.

## Why (verified 2026-08-03 by reading the code)

1. **`crates/recursive-tui/src/backend.rs:1307` (and `:1388`) — the TUI
   hard-aborts the turn task on cancel.** The `wait_for_cancel` select arm
   does `handle.abort(); let _ = handle.await;`. `abort()` drops the task
   in place — whatever the kernel was mid-way through (an LLM stream, a
   tool call) is discarded without running destructors that would emit
   `TurnFinished` / persist messages.

2. **The kernel's cancellation path never fires in the TUI.**
   `runtime.rs:855 set_interrupt_token` installs a `CancellationToken` into
   `kernel.shutdown_token`, which `run_core.rs` polls (the `tokio::select!`
   in `parse_sse_stream`, plus `check_shutdown` at the top of each step).
   The HTTP layer installs one every turn (`http/handlers.rs:910-915`).
   **The TUI never calls `set_interrupt_token`** (grep
   `set_interrupt_token crates/recursive-tui/` → empty), so
   `kernel.shutdown_token` is `None` for TUI runs and the graceful
   `Error::Cancelled` → `make_cancelled_outcome` path is unreachable.

3. **`crates/recursive-tui/src/backend.rs:921-922` — abort rolls back
   memory but not disk.** On the aborted path:
   ```rust
   if aborted {
       recovered.truncate_transcript(pre_turn_len);
   ```
   `truncate_transcript` (`runtime.rs:826`) only mutates the in-memory
   `Arc<Vec<Message>>`. The user message for this turn was *already*
   persisted to `transcript.jsonl` at `runtime.rs:262` (before the LLM
   call). So after an abort: disk has `[…, user_N]`, memory has `[…]`
   (user_N truncated away). The next turn reads context from memory → the
   just-asked question is gone from the model's view, yet it lingers on
   disk to confuse a later `session resume`.

4. **`pre_turn_len` is captured before `enqueue`** (`backend.rs:848-853`),
   so it excludes the current user message — the truncation removes more
   than "this turn's partial work", it removes the user's prompt itself.

## Scope (do exactly this, no more)

### 1. Install a CancellationToken per turn (mirror HTTP)

In `crates/recursive-tui/src/backend.rs`, before spawning each turn task
(the `SendMessage` path at ~line 900, the `RunSkillPrompt` path at ~1112,
and `SetGoal` at ~960), create a fresh `CancellationToken` and install it:

```rust
let interrupt_token = tokio_util::sync::CancellationToken::new();
{
    let mut rt = rt_opt.as_mut().unwrap();
    rt.set_interrupt_token(interrupt_token.clone());
}
// keep interrupt_token alive for the select loop to cancel()
```

Store the token clone alongside the existing `cancel_flag` (or replace the
flag entirely — see step 2) so the `Interrupt` handler can reach it.
Pattern reference: `src/http/handlers.rs:908-915`.

### 2. Interrupt action: cancel the token, not just a flag

Change `UserAction::Interrupt` (`backend.rs:1077-1080`) from setting an
`AtomicBool` to calling `interrupt_token.cancel()` (then notify, as now,
if a select arm still waits on it). The `cancel_flag` `AtomicBool` can be
removed if no other reader depends on it — grep first; if
`run_turn_select_loop` is the only consumer, migrate it off the flag onto
the token.

### 3. run_turn_select_loop: cancel-and-await, not abort

In the `wait_for_cancel` select arm (`backend.rs:1306-1311` and `1387-1392`),
replace:
```rust
handle.abort();
let _ = handle.await;
```
with:
```rust
// Graceful: signal the kernel's CancellationToken so it exits the
// current LLM stream / step via Error::Cancelled → make_cancelled_outcome,
// persisting whatever the turn produced so far. Then await the natural
// (non-aborted) join.
interrupt_token.cancel();
match handle.await {
    Ok(Ok(())) => {}      // turn finished cleanly after cancel
    Ok(Err(e)) => { let _ = event_tx.send(UiEvent::Error { message: e.to_string() }); }
    Err(e) => { tracing::warn!("turn task panicked after cancel: {e}"); }
}
```

The task now exits via the kernel's cancellation path, so
`emit_turn_messages` runs (for the partial turn) and the in-memory
transcript already reflects what was persisted — no manual truncation
needed. Keep the `UiEvent::Interrupted` send.

### 4. Drop the truncate_transcript rollback on the aborted path

Remove the `if aborted { recovered.truncate_transcript(pre_turn_len); }`
block at `backend.rs:921-922` (and the parallel one at `1320-1321` for
`RunSkillPrompt`, and any in `SetGoal`). With graceful cancel the runtime
self-consistently persists its real state; rolling memory back would only
re-introduce the disk/memory divergence. Keep the
`queued_messages.clear()` on a *user-initiated* interrupt if you want to
preserve the "drop type-ahead on cancel" behaviour, but remove it for a
graceful (token-based) cancel where the turn persisted partial work — note
the decision in the journal. (Default: keep clear() — type-ahead queued
during a doomed turn is usually stale.)

### 5. Tests

- `crates/recursive-tui/src/backend.rs` `#[cfg(test)] mod tests`: add a
  test that builds a backend with a stub runtime, drives `UserAction::Interrupt`,
  and asserts the token is cancelled (and the flag, if kept, is set). If a
  full backend test is too heavy, extract the "interrupt handler" logic into
  a small testable function.
- A test (in `crates/recursive-tui/tests/` or backend tests) that verifies
  `set_interrupt_token` is wired before a turn — e.g. assert the token stored
  on the backend is `Some` and fresh after a `SendMessage` is dispatched.
  If introspection is hard, at minimum a grep-verifiable check (Acceptance).
- If `handle.abort()` removal is testable: a regression test asserting the
  cancel path awaits the handle (not aborts). The `pty_regression.rs` suite
  may be the right home — check what it can drive.

## Files NOT to touch

- `src/runtime.rs::set_interrupt_token` — already correct; just call it.
- `src/run_core.rs`, `src/kernel.rs` — cancellation routing is Goal 382's
  domain (or already present); this goal is purely the TUI caller.
- `src/http/**` — already correct; reference only.
- `src/llm/**` — Goal 382.
- `src/session/**` — persistence sink is fine.
- `.dev/flows/`, `.dev/scripts/`, `.flowcast/` — supervisor infrastructure.

## Acceptance

- `cargo build --workspace` green.
- `cargo test --workspace` green.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.
- TUI gates: `sh .dev/scripts/tui-test-presence.sh` green (hard gate);
  `cargo test -p recursive-tui` green; `tui-mutants.sh` run (note any
  surviving mutants in the journal — don't necessarily kill them all if
  they're pre-existing).
- Grep: `rg 'set_interrupt_token' crates/recursive-tui/src/` — the TUI now
  installs a cancellation token each turn (non-empty result).
- Grep: `rg 'handle.abort\(\)' crates/recursive-tui/src/backend.rs` —
  **empty** (abort removed from the cancel path). If any `abort()` remains,
  it must be justified in the journal (e.g. panic safety on Shutdown).
- Grep: `rg 'truncate_transcript\(pre_turn_len\)' crates/recursive-tui/src/` —
  **empty** (the memory-only rollback is gone).
- Headline tests by name:
  `cargo test -p recursive-tui interrupt` — new interrupt-wiring tests green.

## Notes for the agent (traps)

- **Don't remove `cancel_flag` blindly.** Grep every reader of the flag
  before deleting it; if `run_turn_select_loop`'s signature or the Shutdown
  path still reads it, keep it or migrate all readers together. Prefer
  migrating to the token fully, but a partial migration that leaves two
  signals is worse than one clean signal.
- **Three call sites, not one.** `SendMessage` (~900), `RunSkillPrompt`
  (~1112), and `SetGoal` (~960) each spawn a turn task with its own
  `run_turn_select_loop`. All three need the token install + cancel-and-await
  treatment. Missing one leaves a hard-abort path alive.
- **Graceful cancel may take a moment.** Unlike `abort()` (instant), the
  kernel needs to notice the token (next SSE chunk boundary or next step
  top). The user may perceive a brief delay. The `UiEvent::Interrupted`
  should fire after the await, not before — if the UI needs immediate
  feedback, send a separate "cancelling…" event first (note in journal).
- **Keep `queued_messages.clear()` for now** (user-initiated cancel drops
  type-ahead), but document that this is now a product choice, not a
  memory-safety hack. Revisit if users complain.
- **Goal 382 interaction.** This goal lands best after 382 (so a cancel
  mid-stream persists partial content). If 382 isn't merged, this goal
  still helps: completed tool results and earlier-step assistant text in a
  multi-step turn get persisted instead of discarded. The two goals don't
  conflict if landed in either order, but the combined effect (partial
  stream content + graceful cancel) is what fully fixes the user's scenario.
- **cargo-fmt + clippy are enforced gates** — run both before finishing.
- **Journal**: write `.dev/journal/manual-20260803-goal383-tui-graceful-cancel.md`
  with Date / Goal / Files touched / Tests added / Notes (especially the
  abort→cancel timing trade-off and the truncate-removal rationale).
