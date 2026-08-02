# Manual edit: goal-362 — End-to-end tests for multi-agent MessageBus inter-role routing

**Date**: 2026-08-02
**Goal**: Add integration tests in `tests/agent_team_integration.rs` pinning the
`MessageBus` pub/sub contract (`src/multi.rs:166-256`) so a refactor of the broadcast
subscription, history, or capacity logic cannot land green. Test-only goal — production
code (`src/multi.rs`) untouched.

## Files touched

- `tests/agent_team_integration.rs` (+191 lines)
  - Imports: `recursive::message::Role`, `recursive::multi::{AgentMessage, AgentPool,
    AgentRole, MessageBus, MessageType}`, `recursive::{Config, FinishReason}`,
    `tokio::sync::broadcast::error::TryRecvError`, `std::path::PathBuf`.
  - New helpers: `test_config()` (full `Config` literal — the struct has no `Default`;
    mirrors `tests/http_common/mod.rs::mock_config` and `tests/v050_integration.rs`),
    `bus_message(from, to, content)` (constructs an `AgentMessage`).
  - 3 new tests (see below).
- `.dev/journal/manual-20260802-goal362-messagebus-tests.md` (this file).

## Tests added

All in `tests/agent_team_integration.rs`, `#[tokio::test]`:

1. `message_bus_routes_message_to_subscribed_role` — subscribes "worker" AND "reviewer",
   sends `AgentMessage{to: "worker"}` via `MessageBus::send`, asserts the worker rx
   yields the message with payload intact, and the reviewer rx sees NOTHING
   (`TryRecvError::Empty`). Pins send→subscribe routing and role-keyed (non-broadcast)
   delivery.

2. `message_bus_history_replay_surfaces_past_messages` — sends BEFORE any subscriber
   exists, subscribes AFTER, asserts:
   - `rx.try_recv()` → `Empty`: **no replay** of pre-subscription messages through the
     broadcast channel (`subscribe()` is a plain `tx.subscribe()`, tokio broadcast starts
     at the current channel position). Contract pinned: late subscribers don't get
     replay via the rx.
   - `bus.inbox("worker")` DOES contain the message: the bus history retains it. So
     "history" is queryable via `inbox()`/`history()`, not pushed to late subscribers.

3. `run_with_role_includes_bus_message_in_context` — builds an `AgentPool` with a
   "worker" role, `bus.subscribe("worker")`, `bus.send(...MESSAGE_FROM_COORDINATOR...)`,
   runs `pool.run_with_role("worker", ...)` against a one-shot `MockProvider` (stop
   completion, no tool calls), then inspects `provider.calls()` (the mock records the
   exact transcript it was sent). Asserts:
   - run succeeds, `FinishReason::NoMoreToolCalls`, exactly 1 LLM call;
   - system message present with the role prompt;
   - bus message is on the bus (`inbox("worker")`) but **NOT** in the turn context.

## ⚠️ Discovery: real gap, NOT fixed (per goal instructions)

`AgentPool::run_with_role` (`src/multi.rs:352-399`) does **NOT** inject `MessageBus`
state into the agent turn at all. The turn's system prompt is
`role.system_prompt + shared-memory context`; `TurnContext.mailbox` is hardcoded
`None`; there are zero `bus`/`MessageBus`/`AgentMessage` references in the
`run_with_role` body. The `MessageBus` is a standalone pub/sub that nothing reads when
building a turn — a coordinator posting a message to a worker role sees it in
`bus.inbox()` but the worker's LLM never sees it.

The goal's load-bearing test therefore **pins the current observable behaviour**: test 3
asserts the bus message is ABSENT from the worker's transcript, with a comment marking
this as a known gap to be fixed by a follow-up goal (which will flip the assertion to
`contains`). This keeps the tree green while guaranteeing the gap cannot change
silently: any accidental change to `run_with_role`'s context-building that starts
including (or stops excluding) bus state trips the test.

**Recommendation for a follow-up goal**: wire `run_with_role` (or the pool generally)
to append `bus.inbox(role)` messages to the turn context (e.g. as a system block after
the memory context, or via `TurnContext.mailbox`-style injection), then flip test 3's
assertion to `system.content.contains("MESSAGE_FROM_COORDINATOR")`.

## Verification

- `cargo test --test agent_team_integration message_bus` → 2 passed (tests 1 & 2; note
  test 3's name `run_with_role_includes_bus_message_in_context` does NOT contain the
  substring `message_bus`, so the goal's filter command covers tests 1 & 2; the full
  file covers all three).
- `cargo test --test agent_team_integration` → 14 passed (11 existing + 3 new).
- `cargo test --workspace` → green.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` → clean.
- `cargo fmt --all` → clean.

## Notes

- No production code modified; no new deps.
- Test 2's contract decision was driven by reading the code: `subscribe()` uses tokio
  `broadcast::channel(64)` + `tx.subscribe()`, which has no history replay — so the test
  pins "no replay via rx" + "history retained via inbox()".
- Test 3 reuses the established harness (`AgentPool::new(Arc<MockProvider>, Config)` +
  `add_role` + one-shot `Completion{finish_reason: "stop"}`) exactly as
  `run_with_role_succeeds_with_mock`/`agent_pool_includes_memory_context` do, adding
  `provider.calls()` inspection to observe the real surface (the system prompt sent to
  the LLM).
