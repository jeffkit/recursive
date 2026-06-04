# Manual edit: agent-team-improvements

**Date**: 2026-06-04
**Goal**: Three improvements to the multi-agent team system based on code review of `src/multi.rs`, `src/tools/team_manage.rs`, and `src/tools/spawn_worker.rs`.

## Changes

### B — spawn_worker now supports custom AgentPool roles (role_name param)

**Problem**: `team_add_role` and `spawn_worker` were separate systems. Custom roles
defined via `team_add_role` could only be used via `AgentPool.run_with_role`, not via
the more capable `spawn_worker` tool call.

**Fix** (`src/tools/spawn_worker.rs`):
- Added `pool: Option<Arc<RwLock<AgentPool>>>` field to `SpawnWorkerTool`
- Added `with_pool()` builder method
- Added `role_name` parameter to the tool spec
- When `role_name` is set, the matching `AgentRole`'s `system_prompt`, `max_steps`,
  and `allowed_tools` are used; `worker_type` is ignored
- Precedence: explicit `system_prompt` arg > pool role default > worker_type default
- The `type_label` in the result header shows the role name when `role_name` is used
- Child `SpawnWorkerTool` instances (for recursive delegation) also inherit the pool

### A — spawn_workers_parallel: true concurrent worker dispatch

**Problem**: `spawn_worker` was inherently sequential — callers had to call it multiple
times in series for parallel work. Even read-only workers (explore, reviewer) ran one
at a time.

**Fix** (`src/tools/spawn_workers_parallel.rs`):
- New tool that accepts a `tasks` array, resolves all configurations (including
  pool role lookups), then drives all futures via `futures_util::future::join_all`
- Results are collected and sorted by original task index for deterministic output
- Registered in `src/cli/builder.rs` alongside `spawn_worker`
- Supports same parameters as `spawn_worker` per task: `prompt`, `worker_type`,
  `role_name`, `system_prompt`, `max_steps`, `worker_id`
- `futures-util` promoted from optional to always-on dependency (was already in
  the transitive tree via `mcp` and `http` features)

### C — coordinator_system_prompt() for self-organising agent teams

**Problem**: Tools existed but no guidance for a coordinator agent on how to
autonomously design a specialist team.

**Fix** (`src/multi.rs`):
- Added `coordinator_system_prompt() -> &'static str` that returns a structured
  4-step workflow prompt:
  1. Analyse task → identify subtask expertise needed
  2. Call `team_add_role` to create specialists
  3. Call `spawn_workers_parallel` for independent tasks, sequential `spawn_worker`
     for dependent tasks
  4. Synthesise results
- Includes rules for minimum-specialist design, when to use parallel vs sequential,
  and context completeness requirements

## Files touched

- `src/tools/spawn_worker.rs` — role_name support, with_pool builder
- `src/tools/spawn_workers_parallel.rs` — new file
- `src/tools/mod.rs` — pub mod + pub use for new tool
- `src/cli/builder.rs` — register SpawnWorkersParallel when sub-agent is enabled
- `src/multi.rs` — coordinator_system_prompt() function
- `Cargo.toml` — futures-util promoted to non-optional

## Tests added

- `src/tools/spawn_workers_parallel.rs` — 5 unit tests:
  - `parallel_two_explore_workers`
  - `missing_tasks_array_errors`
  - `empty_tasks_array_errors`
  - `task_missing_prompt_errors`
  - `depth_limit_respected`

## Notes

- The `spawn_worker` `is_deferred` method was referenced in the worktree but is not
  a trait method in the current `Tool` trait — removed from `spawn_workers_parallel`.
- `coordinator_system_prompt()` is not auto-injected; the caller must decide to include
  it (e.g. via a `--coordinator` CLI flag or a special preset).
