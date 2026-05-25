# Goal 77 — Background Task Execution

**Roadmap**: Phase 8.3 — Background task execution (fire-and-forget + poll)

**Design principle check**:
- Implemented as: new tool + supporting infrastructure in `src/tools/`.
  Minimal agent loop touch (only to support async results).
- Agent loop unchanged in structure — background tasks are tools.

## Why

Some operations take a long time (builds, tests, deployments). The agent
currently blocks on every tool call. Background task support lets the
agent fire off long-running commands and check back later, enabling more
efficient use of its step budget.

## Scope (do exactly this, no more)

### 1. `src/tools/background.rs` — new module

Create two tools:

#### `background_run` tool
```
Parameters:
  - command: string (required) — shell command to run in background
  - label: string (optional) — human-readable name for the task
```

Behavior:
- Spawn the command via `tokio::process::Command` without waiting
- Store the child process handle in a shared task registry
- Return immediately with a task ID: `"Started background task: {id} ({label})"`

#### `background_check` tool
```
Parameters:
  - task_id: string (required) — ID returned by background_run
```

Behavior:
- Look up the task in the registry
- If still running: return `"Task {id} still running (elapsed: Xs)"`
- If completed: return stdout + stderr + exit code, then remove from registry

### 2. Task Registry

```rust
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct BackgroundTaskRegistry {
    tasks: Arc<Mutex<HashMap<String, BackgroundTask>>>,
}

struct BackgroundTask {
    label: String,
    child: tokio::process::Child,
    started_at: std::time::Instant,
}
```

The registry is shared between the two tools (both get an `Arc` clone).

### 3. Task ID generation

Simple incrementing counter: `"bg-1"`, `"bg-2"`, etc. No need for UUIDs.

### 4. Resource limits

- Maximum 5 concurrent background tasks. Return error if limit reached.
- Each task has a timeout of 300 seconds. If exceeded, kill it and
  report timeout on next `background_check`.

### 5. Tool registration

Register both tools in `src/tools/mod.rs`. They should be available
alongside existing tools.

### 6. Tests

- Test: `background_run` starts a task and returns an ID
- Test: `background_check` on running task reports "still running"
- Test: `background_check` on completed task returns output
- Test: `background_check` on unknown ID returns error
- Test: max task limit is enforced
- Test: task timeout works (use `sleep 999` + short timeout)

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- Agent can fire-and-forget a command and check it later
- No regressions

## Notes for the agent

- Read `src/tools/shell.rs` for how shell commands are spawned.
- Read `src/tools/mod.rs` for how tools are registered.
- The key design: both tools share a `BackgroundTaskRegistry` via `Arc`.
  Create the registry in `main.rs` (or wherever tools are built) and
  pass it to both tool constructors.
- Use `tokio::process::Command::new("sh").arg("-c").arg(&command)` for
  the background command (same as run_shell).
- For `background_check`: use `child.try_wait()` — it returns `None`
  if still running, `Some(status)` if done. Then read stdout/stderr.
- Don't forget to cap output size (same as run_shell: 10000 chars).
