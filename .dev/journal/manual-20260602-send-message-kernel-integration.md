# Manual edit: send_message kernel integration + Agent Team e2e tests

**Date**: 2026-06-02
**Goal**: Complete the `send_message` bidirectional messaging feature and add comprehensive integration tests for the Agent Team multi-agent coordination subsystem.

## Changes summary

### 1. `send_message` Kernel Integration

The coordinator → worker mid-run messaging is now fully wired end-to-end:

- **`src/kernel.rs`**: Added `mailbox: Option<WorkerMailbox>` field to `TurnContext`. The kernel passes it through to `RunCore`.
- **`src/agent.rs`**: Added `mailbox: Option<WorkerMailbox>` to `RunCore`. In `run_inner()`, at the start of each step (before transcript budget check), the kernel calls `mailbox.drain_all()` and appends each pending message as a `Role::User` message prefixed `[coordinator]: ...`, so the LLM sees coordinator instructions on the next reasoning step.
- **`src/runtime.rs`**, **`src/tools/sub_agent.rs`**, **`src/multi.rs`**: Added `mailbox: None` to all existing `TurnContext` construction sites.
- **`src/tools/spawn_worker.rs`**: Major update:
  - Added `registry: Option<WorkerRegistry>` field + `with_registry()` builder method.
  - Removed the stale `mailbox` field (mailbox is now obtained from registry at execute time).
  - Each `execute()` call auto-generates a `worker_id` (UUID v4) and registers the worker in the registry before the kernel run, then deregisters after.
  - Updated output format: `[worker_id:{id} type:{type} finished:{reason}]`.
  - Updated unit test assertions to match new output format.

### 2. Agent Team Integration Tests (`tests/agent_team_integration.rs`)

19 new integration tests covering the full coordinator pattern stack:

**WorkerMailbox tests:**
- `mailbox_fifo_ordering` — FIFO delivery guarantee
- `mailbox_drain_is_destructive` — double drain returns empty
- `mailbox_pop_while_empty_returns_none`

**WorkerRegistry tests:**
- `registry_register_and_send` — push via registry lookup
- `registry_deregister_removes_worker`
- `registry_active_workers_list`
- `registry_concurrent_push_and_drain` — 10-message concurrent push, FIFO order preserved

**SendMessageTool tests:**
- `send_message_tool_delivers_to_registered_worker`
- `send_message_tool_unknown_worker_returns_helpful_error` — lists active workers
- `send_message_tool_spec_has_required_fields` — spec validation

**Kernel mailbox drain test:**
- `worker_receives_coordinator_message_via_mailbox` — end-to-end: mailbox pre-loaded, kernel runs, mailbox empty afterwards

**Dynamic team management tests (TeamAddRole/RemoveRole/ListRoles):**
- `team_add_role_creates_new_role`
- `team_add_role_updates_existing_role`
- `team_remove_role_removes_existing_role`
- `team_remove_role_nonexistent_returns_message`
- `team_list_roles_shows_all_roles`
- `team_list_roles_empty_pool`

**AgentPool lower-level API tests:**
- `agent_pool_add_and_remove_role`
- `agent_pool_remove_nonexistent_returns_false`

## Quality gates

```
cargo test    → 922 passed (lib) + 19 passed (agent_team_integration) = all green
cargo clippy  → clean
cargo fmt     → applied
```

## Files touched

- `src/kernel.rs` — TurnContext.mailbox field
- `src/agent.rs` — RunCore.mailbox field + drain loop in run_inner
- `src/runtime.rs` — TurnContext construction
- `src/tools/sub_agent.rs` — TurnContext construction
- `src/tools/spawn_worker.rs` — registry integration, worker_id, format update
- `src/multi.rs` — TurnContext construction
- `tests/agent_team_integration.rs` — **new** — 19 integration tests

## Notes

The coordinator → worker messaging now works at the protocol level: a coordinator can call `send_message` while a worker is running, and the worker will pick up the message at its next step boundary. The `spawn_worker` tool handles registration/deregistration lifecycle automatically.

The `spawn_worker` output format changed from `[worker:TYPE finished:...]` to `[worker_id:UUID type:TYPE finished:...]` to enable the coordinator to correlate output with a specific worker ID returned from `send_message` registry lookups.
