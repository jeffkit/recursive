# Goal 266 — Coordinator mode: CLI wiring + AgentTool manifest extension

## Summary

Goal 264 landed the library surface for coordinator mode
(`src/coordinator.rs`, `src/team.rs`, `src/tasks.rs`, 8 new tools).
Two pieces were explicitly deferred as requiring a separate design pass:

1. **CLI wiring** — `src/cli/builder.rs` should gate the tool set on
   `RECURSIVE_COORDINATOR_MODE` env var (and the `coordinator-mode`
   cargo feature). When the var is set, replace the default tool set
   with `coordinator_tool_set()` from `src/coordinator.rs`.

2. **AgentTool manifest extension** — the `AgentTool` schema exposed
   to the LLM currently has no fields for `team_name`, `name`
   (logical task name), or `run_in_background`. These are needed so a
   coordinator agent can spawn named sub-agents and route results back
   via `SendMessageTool`. The `with_task_registry` builder is already
   wired; the manifest just needs the new optional fields documented
   and the dispatch path implemented.

## Detailed spec

### 1. CLI wiring (`src/cli/builder.rs`)

- After resolving the tool set, check:
  ```rust
  #[cfg(feature = "coordinator-mode")]
  if src::coordinator::is_coordinator_mode() {
      tool_set = src::coordinator::coordinator_tool_set();
  }
  ```
- The `coordinator_tool_set()` already exists and returns a `Vec<Box<dyn Tool>>`.
- Add an integration test that sets `RECURSIVE_COORDINATOR_MODE=1` and
  verifies the resolved tool set does NOT contain `EditTool` or `WriteTool`.

### 2. AgentTool manifest extension (`src/tools/agent.rs`)

Add optional fields to the JSON schema and `call()` dispatch:

| Field | Type | Meaning |
|-------|------|---------|
| `name` | string (optional) | Logical name for the spawned task (used in `TaskRegistry`) |
| `team_name` | string (optional) | Which team definition file to load from `~/.claude/teams/` |
| `run_in_background` | bool (optional, default false) | If true, return immediately with `task_id`; if false, wait for completion |

When `run_in_background=true`:
- Register the task in `TaskRegistry` (requires `with_task_registry` to
  have been called on the tool).
- Spawn the agent as a Tokio task.
- Return `{ "task_id": "<uuid>", "status": "running" }` immediately.

When `run_in_background=false` (default, current behavior):
- Existing blocking behavior, no change needed.

### 3. Tests

- Unit test: AgentTool with `run_in_background=true` returns a task_id
  and the task appears in `TaskRegistry`.
- Unit test: `team_name` resolves from `~/.claude/teams/` (use
  `RECURSIVE_TEAMS_DIR` env guard, same pattern as 264 tests).

## Acceptance criteria

- `cargo test --workspace` green.
- `cargo clippy --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` no diff.
- Setting `RECURSIVE_COORDINATOR_MODE=1` changes the active tool set
  at runtime (verified by integration test).
- `AgentTool` call with `run_in_background=true` returns `task_id`
  without blocking.

## Complexity: medium

## Out of scope

- Do not add a new CLI flag for coordinator mode; env var is sufficient.
- Do not change `TeamRegistry` or `TaskRegistry` internals beyond
  what's needed for the manifest dispatch.
- Do not add coordinator mode to the default binary feature set;
  keep it opt-in via the `coordinator-mode` cargo feature.
