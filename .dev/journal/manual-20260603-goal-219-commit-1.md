# Manual edit: goal-219-commit-1 (RunCore → AgentEvent)

**Date**: 2026-06-03
**Goal**: Goal 219 Commit 1 — `refactor(kernel): migrate RunCore to emit AgentEvent directly`
**Files touched**:
- `src/agent.rs` → `src/agent/mod.rs` (renamed; `pub mod types; pub use types::*;` re-export added)
- `src/agent/types.rs` (new — 4 keeper types: `PermissionDecision`, `PermissionHook`, `PlanningMode`, `FinishReason`)
- `src/run_core.rs` (16 emit sites migrated; `events: Sender<AgentEvent>`; new `finish_reason_str` helper)
- `src/kernel.rs` (internal StepEvent → AgentEvent bridge removed; `step_events_tx` now passed directly to `RunCore`)
- `src/agent/mod.rs` (legacy `Agent::run` now has a transient AgentEvent → StepEvent bridge for backward compat; bridge deleted in Commit 2)
- `src/event.rs` (added `From<AgentEvent> for StepEvent` impl + test, used by the legacy bridge)

**Tests added**:
- `event::tests::agent_event_to_step_event_conversion` (new) — round-trips the 9 variants `RunCore` actually emits, plus a sanity check on the unhandled-variant fallback

**Notes**:

### Decisions worth recording

1. **`src/agent.rs` → `src/agent/mod.rs` + `src/agent/types.rs` split was needed to host the 4 keepers**, but the move is captured as a delete + new-file pair (git will detect it as a rename on commit). The legacy `Agent` struct stays in `mod.rs` and re-exports the keepers via `pub use types::*;` so downstream `use crate::agent::FinishReason` paths keep working unchanged in Commit 1.

2. **The spec said "delete line 7 `#![allow(deprecated)]` in `run_core.rs`" — I kept it.** `OnMessageFn` is still referenced by `RunCore::on_message` field, and the legacy `Agent::run` is not deleted until Commit 2. Removing the allow now would be a Commit 2 change.

3. **Added a transient `From<AgentEvent> for StepEvent` impl in `event.rs`** so the legacy `Agent::run` can keep its public `events(tx: Sender<StepEvent>)` API. Without it, 6 unit tests in `src/agent/mod.rs` (which set up an events channel and pattern-match on `StepEvent` variants) would have broken. The impl is mapped to be deleted in Commit 2 along with `StepEvent` itself. The `TurnFinished → Finished` mapping loses the typed `FinishReason` (uses `NoMoreToolCalls` as default) — the legacy tests only pattern-match on the variant, not the inner reason.

4. **Replaced the `"finished"` placeholder string** that the old `From<StepEvent>` bridge used (a pre-existing bug — TUI/SDK consumers could never distinguish termination causes) with a proper `finish_reason_str` helper that produces `"no_more_tool_calls"`, `"budget_exceeded"`, `"provider_stop(length)"`, etc. This is a behavior change visible to `AgentEvent::TurnFinished` consumers but invisible to `StepEvent` consumers (the placeholder was only there to satisfy the `String` field of `AgentEvent::TurnFinished`).

5. **The legacy `Agent::run` bridge is fire-and-forget** — it `tokio::spawn`s a converter task and `await`s its handle after `run_inner()` returns. This mirrors the bridge that previously lived in `AgentKernel::run` (which I deleted per the spec). The bridge is invisible when the caller does not set up an events channel (the common case for integration tests).

6. **Untested flaky test `test_load_layered_permissions_session_layer_always_present`** in `src/config_file.rs:307` failed once in the full workspace run but passed on every individual run. The test depends on global state (`~/.recursive/`) and is pre-existing flakey — unrelated to this commit. Will revisit if it surfaces again post-merge.

### What's left for Commit 2

- Delete `Agent` / `AgentBuilder` / `AgentOutcome` / `OnMessageFn` / `StepEvent` from `src/agent/mod.rs` — at that point `mod.rs` collapses to ~3 lines (`pub mod types; pub use types::*;` + file header).
- Delete both `From<StepEvent>` and `From<AgentEvent>` bridges in `event.rs`.
- Delete the `on_message` field from `RunCore` and the corresponding `on_message_callback` from `cost.rs`.
- Update 8 integration test sites + 1 example to use `AgentRuntime::builder()`.
- Switch `HookEvent::SessionEnd { outcome: &AgentOutcome }` → `&RuntimeOutcome`.
- Remove 5 `pub use` re-exports of deprecated types from `lib.rs`.
