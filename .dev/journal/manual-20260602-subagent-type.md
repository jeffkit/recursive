# Manual edit: Goal 175 — `subagent_type` support for parallel explore agents

**Date**: 2026-06-02  
**Goal**: Add named sub-agent personalities (`explore` / `general_purpose`) so that  
read-only explore sub-agents can run **in parallel** via the existing  
`JoinSet`-based parallel dispatch path.  
**Files touched**:
- `src/tools/mod.rs` — added `is_readonly_for_args` to `Tool` trait; added `is_readonly_for_call` to `ToolRegistry`
- `src/tools/sub_agent.rs` — added `AgentType` enum; overrode `is_readonly_for_args`; updated `spec()` with `subagent_type` param; respects agent type in `execute()`
- `src/agent.rs` — both `execute_tool_calls` now use `is_readonly_for_call` instead of `is_readonly`
- `tests/http.rs` — fixed pre-existing `SessionState { title }` missing field
- `src/http.rs` — fixed broken doctest (added `text` fence annotation)
- `.dev/goals/175-subagent-type.md` — goal design document

**Tests added**:
- `tools::sub_agent::tests::explore_agent_type_is_read_only`
- `tools::sub_agent::tests::agent_type_from_str_roundtrip`
- `tools::sub_agent::tests::explore_agent_has_restricted_tool_list`
- `tools::sub_agent::tests::general_purpose_agent_has_no_forced_tool_list`
- `tools::sub_agent::tests::is_readonly_for_args_explore_returns_true`
- `tools::sub_agent::tests::is_readonly_for_args_general_purpose_returns_false`
- `tools::sub_agent::tests::is_readonly_for_args_missing_type_returns_false`
- `tools::sub_agent::tests::is_readonly_for_args_unknown_type_returns_false`
- `tools::sub_agent::tests::explore_agent_dispatch_succeeds`

**Notes**:
- `mcp_e2e` tests fail due to missing local MCP server process — pre-existing,
  unrelated to this change. All 857 lib unit tests green.
- The `is_readonly_for_args` extension is minimal and backward-compatible:
  any tool that doesn't override it delegates to `is_readonly()` as before.
- `explore` agents get a restricted tool list (read-only tools only) enforced
  at dispatch time, regardless of what the LLM passes in the `tools` field.
