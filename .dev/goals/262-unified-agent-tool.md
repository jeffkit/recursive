# Goal 262 — Unify multi-agent spawning into one `Agent` tool (R-2 / R-3)

**Roadmap**: Code quality — architecture review follow-up (P1 backlog,
Phase B of multi-agent unification)

**Design principle check**:
- Implemented as: a single `Agent` tool with parameter-based dispatch
  (replaces 3 near-duplicate tools + 1 orchestration tool)
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- ❌ Does NOT touch `src/tools/a2a.rs` (external agent protocol stays
  as-is — it is the interop surface, not part of the unification)
- ❌ Does NOT add new file types outside `src/tools/`

## Why

The architecture review (`docs/review/architecture-review-2026-06-07.md`,
items R-2 and R-3) flagged that Recursive has **three overlapping ways
to spawn an LLM-driven sub-task**, all called from the LLM tool
surface, with confusingly different semantics:

1. **`sub_agent` tool** (`src/tools/sub_agent.rs`, 638 lines) — generic
   LLM-driven sub-agent. 2 hard-coded agent types
   (`explore`, `general_purpose`). Depth limit, max-steps, tool subset.
2. **`spawn_worker` tool** (`src/tools/spawn_worker.rs`, 557 lines) —
   single-role worker with role-specialist prompt + tool list.
   5 hard-coded worker types. Backed by `multi::AgentPool` + `SharedMemory`.
3. **`spawn_workers_parallel` tool**
   (`src/tools/spawn_workers_parallel.rs`, 578 lines) — fan-out variant
   of spawn_worker over multiple roles, used by the coordinator pattern.
4. **`team_manage` tool** (`src/tools/team_manage.rs`, 512 lines) —
   register/inspect team roles at runtime, mutates the same
   `AgentPool` that `spawn_worker*` reads.

This is fake-cc's `AgentTool` problem in reverse: fake-cc collapsed
**three legacy spawner tools** (general-purpose, statusline-setup,
Explore) into a single `Agent` tool driven by parameter dispatch
(`subagent_type`, `team_name`, `isolation`, `run_in_background`,
`mode`, `cwd`). The unified `Agent` tool is the only tool the LLM
ever calls to spawn a sub-task; the parameters decide whether the
result is a one-shot subagent, a background task, or a persistent
teammate registered to a team.

**Concrete problems with Recursive's current shape**:

- The LLM has to know which of 3 tools to call for "spawn a helper
  that explores" — `sub_agent` with `subagent_type=explore` vs
  `spawn_worker` with a custom role. They share the same backing
  machinery (`AgentPool` + `SharedMemory`) but expose different
  parameters and different result shapes.
- `sub_agent` is the "LLM-facing" tool with a clean signature
  (subagent_type, prompt, description), but `spawn_worker` is
  used by the **coordinator** path (the
  `coordinator_system_prompt` literally says "Call `spawn_workers_parallel`
  (or sequential `spawn_worker`) to dispatch tasks"). These are the
  same operation with different parameter shapes.
- The depth-limit env var
  (`RECURSIVE_SUB_AGENT_DEPTH_LIMIT`) is checked in 2 places
  (`sub_agent.rs:259`, and in `spawn_worker.rs:259` with a comment
  saying "reuse same env var"). Two enforcement points, one knob.
- 3 separate "no tool registered" / depth-limit / max-steps error
  variants. The LLM has to disambiguate them.

The fix mirrors fake-cc: one tool, parameter-based dispatch.

## Scope (do exactly this, no more)

### 1. New file `src/tools/agent_tool.rs` — the unified `Agent` tool

Create one new tool file that owns the unified shape. Tool name
`agent` (singular). Suggested shape, mirroring fake-cc's
`AgentTool.tsx::call()` signature (lines 239-249):

```rust
//! Unified `Agent` tool — replaces `sub_agent`, `spawn_worker`,
//! `spawn_workers_parallel`, and `team_manage`. Reference:
//! fake-cc `src/tools/AgentTool/AgentTool.tsx`.
//!
//! The LLM always calls `agent`. The `subagent_type`, `team_name`,
//! and `run_in_background` parameters decide what kind of spawn
//! happens:
//! - `subagent_type` only → one-shot LLM-driven subagent
//! - `team_name` + `name` → persistent teammate registered to team
//! - `run_in_background: true` → runs as a background task
//!   (returns a task_id immediately; caller polls TaskOutput)

use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct AgentInput {
    /// The user-facing task for the subagent.
    pub prompt: String,
    /// The agent type to spawn. Maps to one of the registered
    /// `AgentDefinition`s (built-in or custom markdown). Examples:
    /// `"general_purpose"`, `"Explore"`, `"Plan"`, or any custom
    /// agent loaded from `~/.claude/agents/*.md` (see Phase C).
    pub subagent_type: String,
    /// Short description of what this subagent will do (used in
    /// tool call list rendering).
    pub description: String,
    /// Optional model override. `None` = inherit parent's model.
    pub model: Option<String>,
    /// Run as a background task; returns `task_id` instead of the
    /// final result. Caller uses `TaskOutput` to fetch later.
    /// Mirrors fake-cc's `run_in_background` param.
    #[serde(default)]
    pub run_in_background: bool,
    /// Teammate name (only when `team_name` is set). The team
    /// roster is flat; teammates cannot spawn other teammates.
    pub name: Option<String>,
    /// If set, spawns as a persistent teammate registered to this
    /// team file (mirrors fake-cc's `team_name` param). If `None`,
    /// this is a one-shot subagent.
    pub team_name: Option<String>,
    /// Permission mode for the spawn (mirrors `PermissionMode`).
    pub mode: Option<PermissionMode>,
    /// Isolation: `"worktree"` runs the agent in a fresh git
    /// worktree, `"none"` runs in the current directory. Mirrors
    /// fake-cc's `isolation: "worktree" | "remote"`. We don't ship
    /// `"remote"` (no CCR backend yet); only `"worktree" | "none"`.
    pub isolation: Option<String>,
    /// Override the working directory for the spawned agent.
    pub cwd: Option<PathBuf>,
}

pub struct AgentTool {
    pool: Arc<AgentPool>,
    agent_defs: Arc<AgentDefinitions>,
    team_registry: Arc<TeamRegistry>, // Phase D introduces this; for now
                                       // fall back to `None` (one-shot only)
}

#[async_trait]
impl Tool for AgentTool {
    fn name(&self) -> &str { "agent" }
    fn aliases(&self) -> &[&str] { &["Task"] } // legacy alias per fake-cc
    // ...
}
```

**Dispatch logic in `call()`**:

1. If `team_name` is set AND `name` is set → spawn persistent teammate
   (Phase D will fill this in; for now, return
   `Err(Error::NotImplemented("team spawn"))` so the path exists but
   errors cleanly until Phase D lands).
2. Else if `run_in_background` is true → spawn as a background task;
   return a `task_id` in the result; the actual LLM invocation
   happens in a tokio task tracked in a `TaskRegistry` (Phase D
   introduces the task-lifecycle tools; for now, run synchronously
   with a warning that the LLM-facing param is plumbed but only the
   synchronous path is live).
3. Else → one-shot subagent (the existing `sub_agent` behavior):
   - Resolve `subagent_type` against `agent_defs` (Phase C will
     populate this from `~/.claude/agents/*.md`; for now, hard-code
     the 2 existing types: `explore`, `general_purpose`).
   - Check depth limit (single source of truth in this file).
   - Spawn via the same backing machinery the old `SubAgent::execute`
     used (refactor the inner loop into a private helper on
     `AgentPool` — see step 3).
   - Honor `isolation = "worktree"` by creating a worktree before
     the spawn and tearing it down after.

**Result shape**: a single `AgentResult` enum:

```rust
pub enum AgentResult {
    /// One-shot subagent completed; `output` is the final assistant
    /// message.
    Completed { output: String, steps: usize },
    /// Background task started; caller polls with `task_id`.
    Background { task_id: String, status: String },
    /// Teammate spawned; `member_id` is the roster entry.
    Teammate { member_id: String, status: String },
}
```

### 2. Delete the 3 old tools + 1 orchestrator tool

After the new `AgentTool` is in place and registered:

- **Delete `src/tools/sub_agent.rs`** (638 lines). The
  one-shot-subagent path moves into `AgentTool::call()` (case 3).
- **Delete `src/tools/spawn_worker.rs`** (557 lines). The
  role-specialist path is a parameter on the new tool
  (`subagent_type`).
- **Delete `src/tools/spawn_workers_parallel.rs`** (578 lines). The
  coordinator path will be re-introduced in Phase D using the
  unified tool's `run_in_background: true` + `team_name` knobs.
- **Delete `src/tools/team_manage.rs`** (512 lines). Role
  registration moves into the new `AgentDefinitions` registry
  (Phase C). For now, hard-coded role-specialist prompts that
  `spawn_worker` used are folded into the new
  `AgentDefinitions::built_in()` constructor.
- **Update `src/tools/mod.rs`**: remove the 4 `pub mod` lines
  (`sub_agent`, `spawn_worker`, `spawn_workers_parallel`,
  `team_manage`) and their `pub use` re-exports. Add
  `pub mod agent_tool;` and `pub use agent_tool::AgentTool;`.
  Remove the `register_spawn_worker`,
  `register_spawn_workers_parallel`, `register_team_manage`
  callers in `cli/builder.rs` and add a single
  `register_agent_tool` call.

**Net change**: -4 files, ~2,285 lines deleted, ~1 new file of
~600 lines, net diff ~-1,685 lines.

### 3. Refactor the `AgentPool` helper

The 3 deleted tools share the actual LLM-call loop. Pull that loop
into a single private method on `multi::AgentPool`:

```rust
impl AgentPool {
    /// Run one LLM-driven turn under the given role, honoring the
    /// permission pipeline and recording audit metadata. Returns
    /// the final assistant message and step count.
    pub(crate) async fn run_role(
        &self,
        role: &AgentRole,
        prompt: &str,
        isolation: Option<IsolationMode>,
    ) -> Result<(String, usize), Error> { ... }
}
```

All 3 old tools currently call `pool.run_with_role(...)` with
slightly different audit / step-count wrappers. Consolidate them
into `run_role` so the new `AgentTool` is the only consumer of
`run_with_role` (which itself can be demoted to
`pub(crate)` after the unification).

### 4. Tests for `AgentTool`

Add `#[cfg(test)] mod tests` to the new file. Required cases:

- `agent_one_shot_returns_completed_result` — `run_in_background`
  unset, `team_name` unset, `subagent_type = "explore"`, mock LLM
  returns a 1-message response, expect
  `AgentResult::Completed { output, steps: 1 }`.
- `agent_unknown_subagent_type_returns_error` —
  `subagent_type = "nonexistent"`, expect
  `Err(Error::UnknownAgentType("nonexistent"))`.
- `agent_depth_limit_enforced` — set depth to 2, call `AgentTool`
  with `subagent_type = "explore"` from a child whose own depth
  is already 2, expect
  `Err(Error::DepthLimitExceeded { max: 2 })`.
- `agent_background_returns_task_id` — `run_in_background = true`,
  expect `AgentResult::Background { task_id, .. }` with a
  non-empty `task_id`.
- `agent_team_name_without_name_returns_error` —
  `team_name = "X"`, `name = None`, expect
  `Err(Error::InvalidAgentInput("team_name requires name"))`.
- `agent_isolation_worktree_creates_and_cleans_up` — set
  `isolation = "worktree"`, run, assert that a worktree was
  created at the expected path and removed after completion.
  Use `tempfile::TempDir` to get a clean git repo.
- `agent_alias_legacy_task_name_works` — call the tool with
  `name = "Task"`, expect it to resolve and execute (mirrors
  fake-cc's `aliases: [LEGACY_AGENT_TOOL_NAME]`).
- `agent_cwd_override_is_honored` — pass `cwd = some_path`, mock
  LLM provider, assert that file operations inside the agent
  resolve under `cwd`.

### 5. Update existing tests

The deleted tools' test files (in `src/tools/sub_agent.rs::tests`,
`src/tools/spawn_worker.rs::tests`,
`src/tools/spawn_workers_parallel.rs::tests`,
`src/tools/team_manage.rs::tests`) move with their tools. Some of
the integration tests in `tests/v050_integration.rs` that exercise
these tools will need to call the new `AgentTool` instead. If a
test relied on `spawn_worker` returning a specific output shape,
update the assertion to the new `AgentResult` shape.

The LLM-facing surface changes from
`"sub_agent"`, `"spawn_worker"`, `"spawn_workers_parallel"`,
`"team_manage"` to a single `"agent"`. Any hard-coded list of tool
names elsewhere in the codebase (e.g., the `tools::all_tools()`
function, the auto-classifier's allow-list, the read-only
predicate) must be updated to:
- Replace the 4 old names with `"agent"`.
- The new `agent` tool is treated as a "spawning" tool: it is
  **never** read-only, even if `subagent_type` resolves to a
  read-only role. (The child agent's read-only-ness is enforced
  inside the child, not at the parent tool call site. The parent
  tool invocation always changes runtime state — at minimum, a
  child session is created.)

### 6. Verify

```bash
cargo test --workspace
cargo test --lib tools::agent_tool
cargo test --lib multi::
cargo test --bin recursive
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all
```

All must be clean. Existing tool-registration tests in
`cli/builder.rs` must continue to pass; the only change is the
list of registered tools (4 names → 1).

## Acceptance

- A single `agent` tool exists in `src/tools/agent_tool.rs` with
  the parameter shape listed in step 1.
- The 4 old tool files (`sub_agent.rs`, `spawn_worker.rs`,
  `spawn_workers_parallel.rs`, `team_manage.rs`) are deleted.
- The net diff is at least -1,500 lines (we delete ~2,285 lines
  and add ~600).
- `src/tools/mod.rs` registers exactly one new tool module:
  `agent_tool`. The 4 old `pub mod` lines are gone.
- `cli/builder.rs` calls exactly one `register_agent_tool` (and
  nothing else for the deleted tool families).
- At least 7 tests in `agent_tool.rs::tests` cover: one-shot path,
  unknown subagent type, depth limit, background, team_name
  without name, isolation worktree, legacy alias.
- `a2a.rs` is unchanged (verifiable via `git diff main..branch --
  src/tools/a2a.rs` showing 0 lines).
- The depth-limit env var is read in **exactly one place** in
  `agent_tool.rs` (currently it's read in 2 places: `sub_agent.rs`
  and `spawn_worker.rs`).
- All quality gates clean.

## Notes for the agent

- **Read first**:
  - `src/tools/sub_agent.rs` (the LLM-facing shape you'll keep)
  - `src/tools/spawn_worker.rs` and `src/tools/team_manage.rs`
    (the role-specialist + role-registration machinery you'll
    fold into the new tool)
  - `src/multi.rs` (the `AgentPool`, `AgentRole`, `SharedMemory`,
    `MessageBus` types your tool will read from)
  - `src/cli/builder.rs` (where the 3 deleted tools are
    registered today; update the registration site)
  - `src/tools/mod.rs:175-220` (the 4 `pub mod` and 4 `pub use`
    lines you'll remove)
- **Read second** (reference):
  - `~/Downloads/fake-cc/src/tools/AgentTool/AgentTool.tsx` —
    the unified-tool design, especially the `call()` parameter
    list (lines 239-249) and the team-vs-subagent branch (lines
    282-316).
  - `~/Downloads/fake-cc/src/tools/AgentTool/loadAgentsDir.ts` —
    the `BaseAgentDefinition` type (lines 106-133) you'll
    eventually back the `subagent_type` resolver with in Phase C.
- **Phase ordering matters**: do **not** add the
  `team_name`-with-`name` (teammate) path. That's Phase D. Wire
  the parameter, validate it, and return
  `Err(Error::NotImplemented("team spawn"))` so the public API
  is locked in, but the implementation is reserved for Phase D.
  Same for `run_in_background: true` — wire the param, but the
  current behavior is to run synchronously (this is the smallest
  safe diff and Phase D adds the task-lifecycle plumbing).
- **Tool registration**: the unified `AgentTool` registers itself
  with name `"agent"` and legacy alias `"Task"`. The legacy alias
  exists so existing prompts that say "use the Task tool" still
  resolve. Both names map to the same handler.
- **`agent_defs` for now**: hard-code the 2 existing built-in
  types (`explore`, `general_purpose`) as
  `AgentDefinitions::built_in()` returning a fixed Vec. Phase C
  will replace this with the `load_agents_dir` reader.
- **Public API surface**: `AgentTool::new(pool, agent_defs)` is
  the only constructor. The `ToolRegistryBuilder` method to
  register it is `with_agent_tool(tool: AgentTool)`. The 4 old
  builder methods are removed.
- **No `src/agent.rs` changes.** All new logic lives in
  `src/tools/agent_tool.rs` and a private helper on
  `src/multi.rs::AgentPool`.

## Out of scope (DO NOT do these)

- Don't implement the `team_name + name` (teammate) path. Wire
  the param, return `Error::NotImplemented`, and stop. Phase D
  fills it in.
- Don't implement `run_in_background: true` semantics. Wire the
  param, run synchronously with a debug log, and stop. Phase D
  fills in the task-lifecycle plumbing.
- Don't add custom agent markdown loading. Phase C does that.
  This goal only unifies the existing 4 tools; the 2 built-in
  types stay hard-coded for now.
- Don't touch `src/tools/a2a.rs`. It is the interop surface
  (external HTTP-based agent protocol) and is explicitly
  excluded from this unification. It is referenced only in
  `a2a.rs`'s own tests; no other tool depends on it.
- Don't change `multi::AgentPool`, `SharedMemory`, `MessageBus`,
  `AgentRole`, or `coordinator_system_prompt` other than the
  `run_role` extraction in step 3 (which is a private helper
  addition, not a behavior change).
- Don't branch inside `src/agent.rs::Agent::run`. All new logic
  lives in `src/tools/agent_tool.rs`.
- Don't add a new error variant for unknown agent type — reuse
  `Error::UnknownAgentType` (already exists) or
  `Error::InvalidInput`. Don't add a depth-limit variant either
  — reuse the existing one in `Error`.
- Don't touch `src/tools/permission_pipeline.rs` (it was just
  extracted in goal 261; leave it alone).
