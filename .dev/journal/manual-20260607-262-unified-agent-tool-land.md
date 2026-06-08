# Manual edit: 262 — unified Agent tool landed

**Date**: 2026-06-07
**Goal**: Land the unified `Agent` tool (goal 262 / Phase B of the
multi-agent unification from the architecture review). Replaces
`sub_agent` + `spawn_worker` + `spawn_workers_parallel` + `team_manage`
with a single parameter-dispatched tool. Mirrors fake-cc's
`AgentTool.tsx::call()` signature.

## Files touched (vs main 47b1b18)

- `src/cli/builder.rs` — 4 register_* callers → 1 register_agent_tool
- `src/error.rs` — no new variants (reused existing `UnknownAgentType` /
  `InvalidInput` / `DepthLimitExceeded`)
- `src/multi.rs` — +32 lines (extracted `AgentPool::run_role` helper)
- `src/tools/agent.rs` — **+819 lines** (new unified `AgentTool`)
- `src/tools/mod.rs` — 4 `pub mod` lines → 1
- `src/tools/sub_agent.rs` — **-638 lines (deleted)**
- `src/tools/spawn_worker.rs` — **-557 lines (deleted)**
- `src/tools/spawn_workers_parallel.rs` — **-578 lines (deleted)**
- `src/tools/team_manage.rs` — **-512 lines (deleted)**
- `tests/agent_team_integration.rs` — imports cleaned up
- `tests/integration.rs` — 19 line refactor for new `AgentResult` shape

**src/ net**: +906 / -2285 = **-1,379 lines** (target was -1,500;
92% of goal — Phase D will delete the team/task tool orphans and
close the gap)

**Branch**: `self-improve/unified-agent-tool-deepseek-pro-20260607T090800Z-90871`
**Merge commit**: `c5c5135` (--no-ff)

## Recovery actions

1. **First deepseek-pro attempt with minimax provider rolled back** at
   step 141 (LLM network error, retries exhausted). The
   `self-improve.sh` verdict handler wrote "rolled-back" to the
   journal but the `git reset --hard` did not run, leaving the
   worktree dirty. Required manual
   `git checkout . && git clean -fd` to recover.

2. **Re-launched with deepseek-pro provider** (per SOP: first rollback
   on a goal = retry with deepseek-pro). Run id
   `unified-agent-tool-deepseek-pro-20260607T090826Z-90930`. Succeeded
   in ~25 minutes (started 09:08:00, completed 09:33ish). All gates
   green from the agent's perspective.

3. **Land script caught a clippy regression the agent missed**:
   `tests/agent_team_integration.rs` had 2 unused imports
   (`recursive::multi::AgentPool`, `tokio::sync::RwLock`) that the
   self-improve gate did not flag during the run. The new
   `land-self-improve.sh` script's gate step caught it on first
   attempt. Fixed with a one-line import cleanup, committed as
   `193a888 fix(tests): drop 2 unused imports flagged by clippy`,
   re-ran gates (all green), then merged.

## Verification

- `cargo test --lib` → 1113 passed, 0 failed
- `cargo test --bin recursive` → 15 passed
- `cargo clippy --all-targets --all-features -- -D warnings` → clean
- `cargo fmt --all -- --check` → clean
- `a2a.rs` diff vs main: 0 lines (untouched, as required)
- Depth-limit env var now read in exactly 1 place
  (`src/tools/agent.rs::AgentTool::call`)

## Notes

- The 2 unused imports the agent left behind are interesting: this is
  the kind of regression that doesn't show up in `cargo test` (which
  the agent ran) but does show up in `cargo clippy` (which the
  self-improve gate is supposed to run but apparently didn't catch).
  This is one of the issues with the self-improve flow being
  investigated separately — see next journal entry.
- The `sub_agent` tool's depth-limit env var is read in exactly one
  place now. The 2-place duplication is gone.
- The `coordinator_system_prompt` still references
  `spawn_workers_parallel` — Phase D (goal 264) will need to update
  that prompt as part of introducing the new team/task tools.

## Next

- Refresh GitNexus index for the new `src/tools/agent.rs` (the
  4 deleted tool files were already in the index; need to add
  `AgentTool` symbols).
- Launch goal 263 (custom agent markdown loading — depends on
  `AgentTool::call` resolving `subagent_type` against a registry).
- Then goal 264 (coordinator mode + team/task tools).
- Investigate and fix the self-improve.sh gate-skipping bug.
