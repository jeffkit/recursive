# Goal 362 — End-to-end test for multi-agent MessageBus inter-role routing

**Roadmap**: Test-coverage hardening — multi-agent message routing flies blind

**Design principle check**:
- Implemented as: NEW integration tests in `tests/agent_team_integration.rs` (test-only).
- ❌ Does NOT modify any production code. `src/multi.rs` is untouched. If a test reveals a
  real bug, STOP and report it (do not fix production here; a separate goal fixes it).
- No new deps.

## Why (the coverage gap, with evidence)

`src/multi.rs:178-260` defines `MessageBus` — the pub/sub layer routing `AgentMessage`
between roles in coordinator/multi-agent mode. It has nontrivial state:
`broadcast::Sender`, a history `VecDeque`, capacity logic at `:207`, and `subscribe()` at
`:216-223` keyed by role name with NO `unsubscribe` (the first subscriber's sender lingers
for the process lifetime).

Coverage today: `src/multi.rs:806` (`run_with_role_succeeds_with_mock`) and `:924`
(`agent_pool_includes_memory_context`) exercise `run_with_role` but only as a
**single-role, single-turn mock returning a stop-completion**. `grep` of
`tests/agent_team_integration.rs` for `MessageBus|run_with_role|run_loop` returns **0 hits**.
The team test file (`:218` `worker_receives_coordinator_message_via_mailbox`) tests the
`WorkerMailbox` — a DIFFERENT, kernel-level FIFO — NOT the `MessageBus` pub/sub.

The blind spot: NO test sends a message from role A through `MessageBus::send` and confirms
it surfaces in role B's turn context. A refactor that breaks the broadcast subscription,
the history replay, or the capacity logic would land green.

## Scope (do exactly this, no more)

Add tests to `tests/agent_team_integration.rs` (extend the existing multi-agent test file).
**Production code is not modified.**

### 1. `message_bus_routes_message_to_subscribed_role`

Minimal pub/sub check:
- Construct an `AgentPool` (or `MessageBus` directly if simpler) with two roles.
- `bus.subscribe("worker")` (or whatever the subscribe API is — read `src/multi.rs:216`).
- `bus.send(AgentMessage { target_role: "worker", ... })`.
- Assert the worker role's receiver actually yields the message (drain the broadcast rx,
  assert the payload matches).

This pins the core send→subscribe contract. Read the `MessageBus` API (`src/multi.rs:178-
260`) for the exact constructor + method signatures before writing.

### 2. `message_bus_history_replay_surfaces_past_messages`

The bus keeps a history `VecDeque` (review cited `:207`). A late subscriber should receive
recent history on subscribe (verify whether this is the actual behaviour — read the code).
Test:
- `bus.send(...)` BEFORE any subscriber exists.
- `bus.subscribe("worker")` AFTER.
- Assert the worker sees the previously-sent message (if history replay exists) — OR if
  the design is "no pre-subscription replay", assert it does NOT see it (pin the contract
  either way; don't assume).

The point is to pin whatever the ACTUAL behaviour is, so a refactor changing it trips this
test. Document in the test comment which contract it's pinning.

### 3. `run_with_role_includes_bus_message_in_context` (the load-bearing one)

This is the end-to-end check that wires the bus into an actual agent turn:
- Build an `AgentPool` with a "worker" role, `bus.subscribe("worker")`.
- `bus.send(AgentMessage { target_role: "worker", content: "MESSAGE_FROM_COORDINATOR" })`.
- `pool.run_with_role("worker", <goal>, mock_provider)` where `mock_provider` returns a
  stop-completion immediately (so the turn is one-shot).
- Assert the worker's injected system context (or transcript) contains the bus message —
  mirror how `agent_pool_includes_memory_context` (`:924`) asserts memory injection.

This is the test that would catch "refactor broke bus message delivery into the agent
turn". Read `run_with_role_succeeds_with_mock` (`:806`) and
`agent_pool_includes_memory_context` (`:924`) for the established harness pattern (how to
build the pool, what mock provider to use, how to inspect the resulting
context/transcript).

### Shared setup

If the three tests duplicate pool/bus construction, extract a small helper
(`fn pool_with_roles(roles: &[&str]) -> AgentPool` or similar) at the top of the new test
block. Do NOT move existing helpers — add a new one if useful, otherwise inline.

## Files NOT to touch

- `src/multi.rs`, any `src/` production code — test-only goal.
- Other test files (`tests/http.rs`, `tests/integration.rs`) — the new tests live in
  `tests/agent_team_integration.rs`.
- `src/run_core.rs`, `src/runtime.rs`, `.dev/flows/`.

## Acceptance

- `cargo test --test agent_team_integration message_bus` — the new tests pass (and existing
  tests in that file still pass).
- `cargo test --workspace` green overall.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.

## Notes for the agent (traps)

- **Read the API before writing.** `src/multi.rs:178-260` defines `MessageBus`; read it IN
  FULL to get the real `subscribe`/`send`/`AgentMessage` signatures. The review's cited
  line numbers are a guide, not gospel — verify.
- **`run_with_role` harness.** The `:806` and `:924` tests already build an `AgentPool` and
  drive `run_with_role` with a mock — copy that setup verbatim, then add the bus message
  injection. Do NOT invent a new pool constructor.
- **Mock provider returns stop-completion.** The turn must be one-shot (no tool calls, no
  loops) so the test is fast and deterministic. Use the same `MockProvider::new(vec![
  Completion { stop }])` pattern the existing tests use.
- **Where the bus message surfaces.** It may be in the system prompt, a system message at
  transcript start, or a dedicated context block — READ how `run_with_role` injects bus
  state (search for `bus`/`MessageBus`/`AgentMessage` references in `src/multi.rs`'s
  `run_with_role` body) before asserting. Assert on the REAL surface, not a guessed one.
- **History replay contract.** If you can't tell from the code whether late subscribers get
  replay, write the test to assert the behaviour you OBSERVE (run the scenario manually via
  the test) and pin THAT. The test's job is to prevent silent change, not to prescribe
  behaviour the code doesn't have.
- **If a test reveals a real routing bug** (e.g. bus.send before subscribe is silently
  dropped when history replay is documented), do NOT fix production in this goal. Mark it
  in the journal and report it so a follow-up goal can fix it. The deliverable is the guard
  test.
