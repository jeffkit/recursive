# Goal 264 — Coordinator mode + team/task tools (Phase D)

**Roadmap**: Code quality — architecture review follow-up (P2 backlog,
Phase D of multi-agent unification)

**Design principle check**:
- Implemented as: a new `team` module (file-backed team registry),
  a new `tasks` module (task lifecycle tracking), and a
  coordinator-mode gate that restricts the LLM's tool set when
  the env var is set
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- ❌ Does NOT touch `src/tools/a2a.rs` (external agent protocol stays
  as-is)
- ❌ Does NOT change the `Agent` tool's public parameter shape (goal
  262 already defined it; this goal wires the implementation of
  the `team_name` + `run_in_background` paths that 262 deferred)

## Why

Goals 262 and 263 unified the LLM-facing agent surface and gave
users a way to define their own agents via markdown. But a real
"coordinator" workflow — the one fake-cc explicitly supports and
Recursive currently has half-built — needs four more capabilities
that the existing code doesn't have:

1. **Persistent teammates**. Goal 262's unified `Agent` tool
   accepts `team_name` + `name` parameters (deferred to Phase D).
   A teammate is a long-lived sub-agent registered to a team,
   addressable by an ID, that the coordinator can `SendMessage` to
   later. The current `spawn_worker*` tools are fire-and-forget;
   they have no roster, no addressability, and no continuation.
2. **Background tasks**. A coordinator workflow fans out research
   to N workers in parallel and continues with the first results
   while the others are still running. This requires the
   `run_in_background: true` parameter (also deferred from 262) to
   actually return a `task_id` immediately and let the caller poll
   for results — not block the entire coordinator turn.
3. **A team file**. fake-cc stores the team roster in
   `~/.claude/teams/{name}.json` so it survives across sessions and
   the coordinator can `SendMessage` to a teammate whose process
   has restarted. The roster is the contract that makes
   `SendMessage(to=...)` resolvable.
4. **A coordinator mode**. The `coordinator_system_prompt()`
   function (already in `src/multi.rs`, used by `cli/builder.rs`)
   tells the LLM how to coordinate, but it works against a tool
   set designed for a single-agent workflow. In coordinator mode
   the LLM should NOT have `Edit`, `Write`, or `Bash` (write-only
   shell) — those would let the coordinator bypass the workers and
   do work itself, undermining the whole pattern. The coordinator
   should also have access to the team-coordination tools
   (`TeamCreate`, `TeamDelete`, `SendMessage`,
   `TaskCreate`/`TaskGet`/`TaskList`/`TaskOutput`/`TaskStop`/
   `TaskUpdate`).

This is the missing layer that turns Recursive's half-built
multi-agent surface into a real "team of agents" workflow.

fake-cc implements this as:
- `src/coordinator/coordinatorMode.ts` — the `isCoordinatorMode()`
  gate and the restricted tool set (`INTERNAL_WORKER_TOOLS`).
- `src/tools/TeamCreateTool/`, `src/tools/TeamDeleteTool/` — the
  team file management.
- `src/tools/SendMessageTool/` — teammate addressing.
- `src/tools/TaskCreateTool/`, `src/tools/TaskGetTool/`,
  `src/tools/TaskListTool/`, `src/tools/TaskOutputTool/`,
- `src/tools/TaskStopTool/`, `src/tools/TaskUpdateTool/` — task
  lifecycle observability.

The shape is well-understood; this goal is a translation, not a
redesign.

## Scope (do exactly this, no more)

### 1. New file `src/team.rs` — the team-file registry

Create a new module that owns the on-disk team-file format. The
file path is `~/.claude/teams/{team_name}.json`. The format is a
flat roster (one entry per teammate), matching fake-cc's
`TeamFile.members`.

```rust
//! On-disk team registry. File format:
//! `~/.claude/teams/{name}.json`. The file is the source of
//! truth for which teammates are alive in a team, what their
//! agent_type is, and their last-known status. Reference:
//! fake-cc `src/tools/TeamCreateTool/`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TeammateId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamMember {
    pub agent_id: TeammateId,
    pub name: String,           // human-readable, unique within team
    pub agent_type: String,    // maps to AgentDefinition
    pub status: TeammateStatus,
    pub created_at: i64,        // unix millis
    pub last_heartbeat_ms: i64,
    pub session_id: Option<String>, // the inner Recursive session
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TeammateStatus {
    Active,
    Idle,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamFile {
    pub team_name: String,
    pub created_at: i64,
    pub members: Vec<TeamMember>,
}

pub struct TeamRegistry {
    base_dir: PathBuf, // ~/.claude/teams/
}

impl TeamRegistry {
    pub fn new() -> Result<Self, Error> { ... }
    pub fn load(&self, team_name: &str) -> Result<TeamFile, Error> { ... }
    pub fn save(&self, team: &TeamFile) -> Result<(), Error> { ... }
    pub fn list(&self) -> Result<Vec<String>, Error> { ... } // team names
    pub fn delete(&self, team_name: &str) -> Result<(), Error> { ... }
    pub fn add_member(&self, team: &str, m: TeamMember) -> Result<(), Error> { ... }
    pub fn remove_member(&self, team: &str, id: &TeammateId) -> Result<(), Error> { ... }
}
```

The file is JSON. The team-name uniqueness invariant is enforced
at `save()` time: if the file already exists with a different
`team_name` field, return
`Error::TeamAlreadyExists(team_name)`. The `members` list is
**flat** — fake-cc's comment in `AgentTool.tsx:266-274` is
relevant: "Teammates cannot spawn other teammates — the team
roster is flat."

### 2. New file `src/tasks.rs` — task lifecycle tracking

A task is a runtime abstraction for anything the coordinator
spawns that produces a result asynchronously. The minimum useful
shape:

```rust
//! Task lifecycle. A Task is a runtime handle to a piece of
//! async work — typically a background agent spawn, but also
//! a long-running shell, a CI watcher, etc. Reference: fake-cc
//! `src/tasks/types.ts::TaskState`.

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

pub type TaskId = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TaskStatus {
    Running,
    Completed,
    Failed,
    Killed,
}

pub enum TaskState {
    /// A background agent invocation.
    Agent {
        id: TaskId,
        description: String,
        started_at: i64,
        finished_at: Option<i64>,
        status: TaskStatus,
        result: Option<String>,
        agent_id: Option<TeammateId>, // for teammate-spawned tasks
    },
    /// A long-running shell command. Out of scope for the first
    /// cut; the enum has a variant so the LLM-facing API can
    /// support it later.
    Shell {
        id: TaskId,
        command: String,
        // ...
    },
}

pub struct TaskRegistry {
    by_id: Arc<Mutex<HashMap<TaskId, Arc<Mutex<TaskState>>>>>,
}

impl TaskRegistry {
    pub fn new() -> Self { ... }
    pub fn create(&self, description: String) -> TaskId { ... }
    pub fn get(&self, id: &TaskId) -> Option<Arc<Mutex<TaskState>>> { ... }
    pub fn list(&self) -> Vec<TaskId> { ... }
    pub fn stop(&self, id: &TaskId) -> Result<(), Error> { ... }
    pub fn update(&self, id: &TaskId, f: impl FnOnce(&mut TaskState)) -> Result<(), Error> { ... }
}
```

The registry is in-memory. A future goal can persist it
(`~/.claude/tasks/{id}.json`), but for now coordinator sessions
are short enough that in-memory is sufficient. The `Arc<Mutex<…>>`
shape is what the LLM-facing tool needs to read partial state
(TaskOutput can poll a long-running task without holding a global
lock).

### 3. Wire `team_name` + `run_in_background` into the unified `AgentTool`

Open the file created in goal 262 (`src/tools/agent_tool.rs`) and
fill in the two cases that 262 deferred to "Error::NotImplemented":

**Case 1: `team_name` + `name` (teammate spawn)**:

```rust
if let (Some(team), Some(name)) = (&input.team_name, &input.name) {
    // 1. Load the team file
    let mut team_file = team_registry.load(team)?;
    // 2. Generate a new TeammateId
    let agent_id = TeammateId(uuid::Uuid::now_v7().to_string());
    // 3. Resolve the agent_type against agent_defs
    let def = agent_defs.get(&input.subagent_type)?;
    // 4. Spawn the inner Recursive session in a background task
    let task_id = task_registry.create(format!(
        "{} ({})",
        name, input.subagent_type
    ));
    let inner_session = spawn_inner_session(def, input.prompt.clone(), input.cwd.clone());
    let task_handle = tokio::spawn(async move {
        // Run the agent, capture result, mark task completed
        match inner_session.await {
            Ok(output) => { /* update task with output */ }
            Err(e) => { /* update task with failure */ }
        }
    });
    // 5. Register the teammate in the team file
    team_file.add_member(TeamMember {
        agent_id: agent_id.clone(),
        name: name.clone(),
        agent_type: input.subagent_type.clone(),
        status: TeammateStatus::Active,
        created_at: now_ms(),
        last_heartbeat_ms: now_ms(),
        session_id: Some(task_id.clone()),
    });
    team_registry.save(&team_file)?;
    // 6. Track the task handle so TaskStop can cancel it
    task_registry.track_handle(&task_id, task_handle);
    // 7. Return
    return Ok(AgentResult::Teammate {
        member_id: agent_id.0,
        status: "spawned".to_string(),
    });
}
```

The `spawn_inner_session` helper is the new bit: it constructs a
fresh `AgentRuntime` (or the `multi::AgentPool::run_role` helper
from goal 262) and runs the LLM loop on a child session ID. The
inner session has its own transcript, its own tool registry (with
the agent's `tools` filter applied), and its own permission
mode. The session ID is stored in the team file so
`SendMessage(to=agent_id)` can resolve it back to the right
runtime.

**Case 2: `run_in_background: true` (background subagent)**:

```rust
if input.run_in_background {
    let task_id = task_registry.create(input.description.clone());
    let def = agent_defs.get(&input.subagent_type)?;
    let task_registry_for_task = task_registry.clone();
    let task_id_for_task = task_id.clone();
    tokio::spawn(async move {
        match run_agent_blocking(def, input.prompt, input.cwd).await {
            Ok(output) => task_registry_for_task.update(&task_id_for_task, |t| {
                if let TaskState::Agent { status, result, finished_at, .. } = t {
                    *status = TaskStatus::Completed;
                    *result = Some(output);
                    *finished_at = Some(now_ms());
                }
            }),
            Err(e) => task_registry_for_task.update(&task_id_for_task, |t| {
                if let TaskState::Agent { status, finished_at, .. } = t {
                    *status = TaskStatus::Failed;
                    *result = Some(format!("Error: {e}"));
                    *finished_at = Some(now_ms());
                }
            }),
        }
    });
    return Ok(AgentResult::Background {
        task_id,
        status: "running".to_string(),
    });
}
```

This is the case that goal 262 deferred with a synchronous
fallback. With the `TaskRegistry` in place, the path is finally
fully async. The caller (typically the coordinator) gets a
`task_id` back and uses `TaskOutput(task_id)` to poll.

### 4. New tool files for the team/task surface

Create one new tool file per tool, mirroring fake-cc's
one-file-per-tool convention:

- `src/tools/team_create.rs` — `TeamCreateTool`. Input:
  `{ team_name: String, description: Option<String>, agent_type: Option<String> }`.
  Output: `{ team_name: String, file_path: String }`.
- `src/tools/team_delete.rs` — `TeamDeleteTool`. Input:
  `{ team_name: String }`. Output: `{ deleted: bool }`.
- `src/tools/send_message.rs` — `SendMessageTool`. Input:
  `{ to: String, message: String }` where `to` is either a
  teammate's `agent_id` (resolvable via the team file) or a
  task_id (resolvable via the TaskRegistry). Output:
  `{ delivered: bool, response_task_id: String }`. The
  implementation: load the team file, find the teammate by id,
  push the message into the inner session's input queue, return
  a new task_id that the caller can poll with `TaskOutput`.
- `src/tools/task_create.rs` — `TaskCreateTool`. Input:
  `{ subject: String, description: String, active_form: Option<String> }`.
  Output: `{ task_id: TaskId }`. Implementation: calls
  `TaskRegistry::create`. **Important**: this is the LLM's
  todo-list tool, **not** a teammate tracker. The `TaskRegistry`
  also tracks teammate spawns (the cases above), so the
  `TaskCreate` output reuses the same id space.
- `src/tools/task_get.rs` — `TaskGetTool`. Input:
  `{ task_id: TaskId }`. Output: `TaskState` JSON.
- `src/tools/task_list.rs` — `TaskListTool`. Input: `{}`. Output:
  `Vec<TaskSummary>` (id, status, description, started_at).
- `src/tools/task_output.rs` — `TaskOutputTool`. Input:
  `{ task_id: TaskId, blocking: bool, max_wait_ms: Option<u64> }`.
  Output: `{ status: TaskStatus, output: Option<String> }`. If
  `blocking: true` and the task is still running, wait up to
  `max_wait_ms` (default 30s) for completion.
- `src/tools/task_stop.rs` — `TaskStopTool`. Input:
  `{ task_id: TaskId }`. Output: `{ stopped: bool }`. Calls
  `TaskRegistry::stop`, which cancels the underlying
  `tokio::task::JoinHandle`.
- `src/tools/task_update.rs` — `TaskUpdateTool`. Input:
  `{ task_id: TaskId, status: Option<TaskStatus>, description: Option<String> }`.
  Output: `{ updated: bool }`.

The `send_message.rs` file is also referenced by the existing
`src/tools/send_message.rs` (used by the spawn_worker family).
This is a **conflict**: the existing file is the legacy
`WorkerMailbox` for the spawn_worker* tools, which are being
deleted in goal 262. After goal 262, the existing
`send_message.rs` is unused and can be **replaced** by the new
one. Verify with `git grep "src/tools/send_message.rs"` after
goal 262's merge that no other file references the old types
(`WorkerMailbox`, `WorkerRegistry`); if clean, overwrite the
file with the new `SendMessageTool` for the team workflow.

### 5. Coordinator mode

Add a feature gate and env-var check mirroring fake-cc's
`coordinatorMode.ts:isCoordinatorMode()`:

```rust
// src/coordinator.rs (new top-level module — not under src/tools/)

pub const COORDINATOR_MODE_ENV: &str = "RECURSIVE_COORDINATOR_MODE";

pub fn is_coordinator_mode() -> bool {
    std::env::var(COORDINATOR_MODE_ENV)
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// Tools the coordinator LLM has access to. Mirrors fake-cc's
/// `INTERNAL_WORKER_TOOLS` set + restricted `AgentTool`
/// behavior. Edit/Write/Bash are EXCLUDED; the coordinator
/// delegates, doesn't act.
pub fn coordinator_tool_set() -> ToolSet {
    let mut set = ToolSet::new();
    set.allow("Read");
    set.allow("Grep");
    set.allow("Glob");
    set.allow("Agent");
    set.allow("SendMessage");
    set.allow("TeamCreate");
    set.allow("TeamDelete");
    set.allow("TaskCreate");
    set.allow("TaskGet");
    set.allow("TaskList");
    set.allow("TaskOutput");
    set.allow("TaskStop");
    set.allow("TaskUpdate");
    set
}
```

Wire this into `src/cli/builder.rs`:

```rust
let tool_set = if coordinator::is_coordinator_mode() {
    coordinator::coordinator_tool_set()
} else {
    // existing default tool set, unchanged
    default_tool_set()
};
```

The system prompt for coordinator mode is the existing
`multi::coordinator_system_prompt()`. It is already injected by
`cli/builder.rs:344`; the goal just gates the tool set on the
same condition.

**Feature flag** (mirroring fake-cc's `feature('COORDINATOR_MODE')`):
the env var only takes effect when the binary was built with the
`coordinator-mode` cargo feature:

```rust
#[cfg(feature = "coordinator-mode")]
pub fn is_coordinator_mode() -> bool { ... }

#[cfg(not(feature = "coordinator-mode"))]
pub fn is_coordinator_mode() -> bool { false }
```

Add the feature to `Cargo.toml`:
```toml
[features]
coordinator-mode = []
```

Default: feature off. Users who want the coordinator workflow
build with `cargo build --features coordinator-mode` (or it's
flipped on by default in a future goal after the workflow is
stable).

### 6. Tests

Add tests for each new module:

**`src/team.rs::tests`** (8 cases):
- `team_registry_create_writes_file`
- `team_registry_load_round_trips`
- `team_registry_load_missing_returns_error`
- `team_registry_add_member_increments_count`
- `team_registry_remove_member_decrements_count`
- `team_registry_delete_removes_file`
- `team_registry_save_rejects_rename_via_field_mismatch`
- `team_registry_list_returns_all_teams`

**`src/tasks.rs::tests`** (6 cases):
- `task_registry_create_returns_unique_id`
- `task_registry_get_returns_state`
- `task_registry_list_includes_all_created`
- `task_registry_update_mutates_state`
- `task_registry_stop_cancels_handle`
- `task_registry_get_unknown_returns_none`

**`src/tools/agent_tool.rs::tests` (additions to goal 262's
tests, 4 cases)**:
- `agent_team_spawn_registers_member_in_team_file`
- `agent_team_spawn_duplicate_name_rejected` — same `name` in
  same team returns `Err(Error::DuplicateTeammateName)`.
- `agent_background_returns_task_id_with_running_status` —
  differs from goal 262's test in that the task is **actually
  running** (poll `TaskRegistry::get` immediately after the tool
  call and assert status is `Running`, not `Completed`).
- `agent_send_message_to_teammate_resolves_via_team_file`

**`src/coordinator.rs::tests`** (4 cases):
- `coordinator_mode_disabled_by_default`
- `coordinator_mode_enabled_with_env_var`
- `coordinator_mode_disabled_when_feature_off_even_with_env`
- `coordinator_tool_set_excludes_write_tools`

### 7. Update existing tests

After goal 262's `agent_tool.rs` was created with deferred cases
returning `Error::NotImplemented`, those tests need to be
**re-enabled** with the new working implementation. The two
specific tests from goal 262's spec that 262 marked as
"deferred":

- `agent_team_spawn_*` — the goal-262 test that calls
  `team_name = "X"`, `name = "worker"`, and expects
  `Err(Error::NotImplemented)` should be **deleted** and replaced
  with the four new tests in step 6.
- `agent_background_returns_task_id` — the goal-262 test that
  expects a synchronous fallback should be **updated** to expect
  the new async-running task with `TaskStatus::Running`.

If any other tests reference the old `WorkerMailbox` /
`WorkerRegistry` types (deleted in goal 262), update them to use
the new `TaskRegistry` / `TeamRegistry` types.

### 8. Verify

```bash
cargo test --workspace
cargo test --lib team:: tasks:: coordinator:: tools::agent_tool
cargo test --bin recursive
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all
```

All must be clean. The 22+ new tests pass; the 7+ goal-262 tests
continue to pass with the deferred cases now implemented.

## Acceptance

- A new `src/team.rs` module owns the team-file registry with
  `TeamFile`, `TeamMember`, `TeammateStatus`, and `TeamRegistry`
  (with `load` / `save` / `list` / `delete` / `add_member` /
  `remove_member`).
- A new `src/tasks.rs` module owns task lifecycle with
  `TaskState`, `TaskStatus`, `TaskRegistry` (with `create` /
  `get` / `list` / `stop` / `update` / `track_handle`).
- A new `src/coordinator.rs` module owns the `is_coordinator_mode`
  gate and the `coordinator_tool_set()` allow-list.
- 9 new tool files exist under `src/tools/`:
  `team_create`, `team_delete`, `send_message` (replacing the
  old one), `task_create`, `task_get`, `task_list`,
  `task_output`, `task_stop`, `task_update`.
- The unified `AgentTool` from goal 262 implements the
  `team_name + name` (teammate) and `run_in_background: true`
  paths that 262 deferred. The "NotImplemented" stubs are
  gone.
- Coordinator mode is gated on
  `RECURSIVE_COORDINATOR_MODE=1` env var AND the
  `coordinator-mode` cargo feature. The tool set in coordinator
  mode excludes `Edit`, `Write`, and write-only `Bash`.
- 22+ new tests across the 4 new modules. All quality gates
  clean.
- `a2a.rs` is unchanged.

## Notes for the agent

- **Read first**:
  - `src/team.rs` (this goal creates it) — model after
    fake-cc's `src/services/team/` directory (not in the
    fake-cc reference we have, but the file format is
    specified in `src/tools/TeamCreateTool/`).
  - `src/tasks.rs` (this goal creates it) — model after
    fake-cc's `src/tasks/types.ts` (read in earlier analysis).
  - `src/coordinator.rs` (this goal creates it) — model after
    fake-cc's `src/coordinator/coordinatorMode.ts`.
  - `src/cli/builder.rs` — where the new tools are
    registered and the `coordinator_tool_set()` is wired.
  - `src/tools/agent_tool.rs` (from goal 262) — the
    deferred paths this goal fills in.
  - `src/tools/send_message.rs` (existing) — verify it's
    safe to overwrite (no callers outside the spawn_worker
    family, which is gone after 262).
- **Lock discipline for `TaskRegistry`**: the registry holds
  `Arc<Mutex<TaskState>>` per task. When you call
  `task_registry.update(&id, |t| { ... })`, hold the inner
  mutex only for the duration of the closure. Do not hold
  the registry's outer `Mutex<HashMap>` across an `await`.
  This mirrors the rule the `PermissionPipeline` already
  enforces (see CLAUDE.md invariant #1).
- **Team file write atomicity**: the existing
  `src/storage/local.rs` has a `safe_write` helper (added in
  P2 for atomic local-storage writes). Use it for the team
  file so a crash mid-write doesn't corrupt the roster:
  ```rust
  let tmp = path.with_extension("json.tmp");
  std::fs::write(&tmp, serde_json::to_vec_pretty(&team)?)?;
  std::fs::rename(&tmp, &path)?;
  ```
- **`SendMessage` resolution order**: when `to = "xyz"` is
  given, try in order:
  1. `TaskRegistry::get("xyz")` — direct task id.
  2. `TeamRegistry::load(team).members.find(m => m.agent_id == "xyz")`
     — teammate id (requires knowing the current team; if not
     in team context, return
     `Err(Error::SendMessageRequiresTeamContext)`).
  The first match wins. If neither matches, return
  `Err(Error::UnknownRecipient("xyz"))`.
- **The "flat team roster" invariant**: a teammate cannot
  spawn another teammate. If `team_name` is set in the
  `SendMessage` tool's caller context (i.e., the caller is
  already a teammate), and the call's input also has
  `team_name`, reject with
  `Err(Error::TeammateCannotSpawnTeammate)`. This is the
  fake-cc comment in `AgentTool.tsx:266-274` translated to
  Rust.
- **Don't ship a "remote" isolation mode**: the unified
  `AgentTool`'s `isolation` parameter accepts `"worktree"`
  only. The `"remote"` value from fake-cc requires a CCR
  backend we don't have. The string is reserved for
  future use; reject any other value with
  `Err(Error::InvalidIsolation(value))`.
- **No public API change to `multi::coordinator_system_prompt()`**:
  the existing function is unchanged. The tool set is
  what's new.

## Out of scope (DO NOT do these)

- Don't add plugin-agent support. Goal 263 left the
  `Plugin` variant as a stub; this goal doesn't fill it.
- Don't persist the `TaskRegistry` to disk. In-memory only;
  coordinator sessions are short. A future goal can add
  `~/.claude/tasks/{id}.json` persistence.
- Don't add a TUI for the team roster. The LLM-facing tools
  are the only interface; humans interact via the
  coordinator's natural-language output.
- Don't change the `Agent` tool's public parameter shape
  (from goal 262). This goal only fills in the implementation
  of the parameters that 262 deferred.
- Don't touch `src/tools/a2a.rs`. External agent protocol is
  out of scope.
- Don't change `multi::coordinator_system_prompt()`. The
  function is reused as-is.
- Don't add observability/metrics for task durations. A
  separate goal can wire tracing spans for the task
  lifecycle.
- Don't branch inside `src/agent.rs::Agent::run`. All new
  logic lives in `src/team.rs`, `src/tasks.rs`,
  `src/coordinator.rs`, and the new tool files.
