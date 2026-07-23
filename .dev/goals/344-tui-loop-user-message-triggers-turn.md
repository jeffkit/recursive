# Goal 344 — TUI loop: user message must trigger a turn, not queue behind the next event

**Roadmap**: TUI loop-supervise correctness (event-driven loop arbiter)

**Design principle check**:
- Implemented as: a behaviour change in `crates/recursive-tui/src/backend.rs::loop_arbiter`
  only — the `UserAction::SendMessage` arms return `ArbiterDecision::Run` instead of
  pushing to `queued_messages` and returning `Idle`.
- ❌ Does NOT touch `src/run_core.rs::RunCore::run_inner` (invariant #1).
- ❌ Does NOT introduce a new `Error` variant (invariant #7).
- ❌ Does NOT change tool-call ↔ tool-result pairing (invariant #8).

## Why

In the TUI event-driven `/loop`, when the agent has armed `schedule_wakeup` /
`watch_file` and is parked in `loop_arbiter` waiting for an event, a user
message submitted via the input box does **not** wake the agent. Observed in
production on 2026-07-23 (deepseek-v4-flash supervisor on Goal 331):

- Agent ended turn 5 with `watch_file(events.jsonl)` + `schedule_wakeup(300s)`.
- User typed "你看不到实际的进展，判断不了为什么挂起吗？" and hit Enter.
- Agent did not respond for ~5 minutes, then woke on the **300s heartbeat**,
  ran the wakeup turn (whose prompt was the `schedule_wakeup` payload
  "Check progress of the Goal 331 self-improve flow…", NOT the user's text),
  and only after that turn did the user's message get processed.

Root cause: `loop_arbiter` treats `UserAction::SendMessage` and
`UserAction::LoopTrigger` asymmetrically.

```backend.rs:437:447
            Ok(UserAction::SendMessage(text)) => {
                queued_messages.push_back(text);
                // Continue draining in case there's more.
            }
            Ok(UserAction::LoopTrigger { source, prompt }) => {
                return ArbiterDecision::Run {
                    prompt: format!("[trigger:{source}] {prompt}"),
                    source,
                    delay_secs: None,
                };
            }
```

`LoopTrigger` returns `Run` (immediate turn). `SendMessage` pushes to
`queued_messages` and falls through. The blocking `select!` arm
(`backend.rs:470-473`) does the same: `SendMessage` → push + `Idle`.
`Idle` in `worker_loop` (`backend.rs:679-681`) is `send(LoopIdle); continue;`
— no turn runs. Worse, the arbiter's Priority-1 drain (`backend.rs:432-454`)
moves any already-queued user message into `queued_messages` and then **breaks
to `select!`**, so a message that arrived before the arbiter was entered is
also stuck behind the next loop event.

Result: a user message submitted while the loop is parked only runs when the
next loop event (wakeup / watch-file / bg-completion) triggers a turn, at
which point `worker_loop`'s top-of-loop `queued_messages.pop_front()` finally
drains it. This contradicts the `loop-supervise` skill contract: "the user's
reply drives the next turn."

## Scope (do exactly this, no more)

### 1. `crates/recursive-tui/src/backend.rs` — `SendMessage` returns `Run`

In `loop_arbiter`, make `UserAction::SendMessage` drive an immediate turn,
symmetric with `LoopTrigger`.

**Priority-1 drain arm** (`backend.rs:437`):

```rust
// before
Ok(UserAction::SendMessage(text)) => {
    queued_messages.push_back(text);
    // Continue draining in case there's more.
}
// after
Ok(UserAction::SendMessage(text)) => {
    return ArbiterDecision::Run {
        prompt: text,
        source: "user".to_string(),
        delay_secs: None,
    };
}
```

**`select!` arm** (`backend.rs:470`):

```rust
// before
Some(UserAction::SendMessage(text)) => {
    queued_messages.push_back(text);
    ArbiterDecision::Idle
}
// after
Some(UserAction::SendMessage(text)) => {
    ArbiterDecision::Run {
        prompt: text,
        source: "user".to_string(),
        delay_secs: None,
    }
}
```

Do NOT change `LoopTrigger`, `StopLoop`/`Interrupt`/`Shutdown`, `StartLoop`,
or the `Forward(other)` arm. Do NOT touch the bg-complete / event-watch /
wakeup branches.

### 2. Type-ahead during a running turn is unchanged

`queued_messages` is still drained at the top of `worker_loop`
(`backend.rs:630-636`) for messages submitted **while a turn is already
running** (genuine type-ahead). That path is untouched and still works: a
message queued mid-turn is run as the next turn after the current one
finishes. The fix only affects the case where the loop is **parked in the
arbiter** with no turn running.

### 3. Tests

In the existing `mod tests` block in `backend.rs` (the `Loop arbiter tests`
section near `backend.rs:1712`), add:

- `user_sendmessage_in_drain_triggers_run_not_idle` — feed a
  `UserAction::SendMessage("hi")` into `action_rx` **before** calling
  `loop_arbiter`, assert the decision is `ArbiterDecision::Run { source: "user", prompt: "hi", delay_secs: None }`
  and that `queued_messages` is **empty** (not enqueued).
- `user_sendmessage_in_select_triggers_run_not_idle` — call `loop_arbiter`
  with an empty channel, then send `UserAction::SendMessage("yo")` from a
  spawned task, assert `Run { source: "user", prompt: "yo" }`.
- `looptrigger_still_triggers_run` — regression guard: `LoopTrigger` still
    returns `Run` with the `[trigger:{source}]` prompt format (unchanged).
- `stoploop_still_stops` — regression guard: `StopLoop` still returns `Stop`.

Use the same test scaffolding as the existing `start_loop_emits_loop_started_and_runs_turn`
test (`backend.rs:1728+`) — `mpsc::unbounded_channel`, a real `WakeupSlot`,
a `BackgroundJobManager`, etc. If the existing tests construct `loop_arbiter`
via a helper, reuse it; otherwise inline the wiring.

## Acceptance

- `cargo test -p recursive-tui` green, including the new tests.
- `cargo clippy --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.
- `loop_arbiter` returns `ArbiterDecision::Run { source: "user", .. }` for
  `UserAction::SendMessage` in both the drain arm and the `select!` arm.
- `LoopTrigger` / `StopLoop` / `StartLoop` / `Forward` behaviour unchanged.
- TUI gates pass: `.dev/scripts/tui-test-presence.sh` exits 0 (new tests
  added in `backend.rs`), and `.dev/scripts/tui-mutants.sh` is advisory but
  survivors inside the changed `loop_arbiter` hunks must be fixed.

## Notes for the agent

- This is a TUI-only behaviour fix. Do NOT modify `src/`, `src/runtime.rs`,
  `src/run_core.rs`, or any tool. Only `crates/recursive-tui/src/backend.rs`.
- The `queued_messages` type-ahead path (top of `worker_loop`) must stay —
  messages sent while a turn is running are still buffered. Only the
  arbiter-parked case changes.
- Keep the `biased` ordering of the `select!` (`action` first) — user input
  must still preempt watch/wakeup/bg.
- Journal entry: `.dev/journal/manual-<YYYYMMDD>-tui-loop-user-message-triggers-turn.md`.
- Reproduction for verification: in the TUI, `/loop start <goal>`, let the
  agent arm a wakeup, then type a message and confirm the agent responds
  immediately (not after the wakeup delay).
