# Manual edit: sdk-params-align

**Date**: 2026-06-04
**Goal**: Align HTTP API and Python/TypeScript SDK with new CLI parameters added in `feat/cli-align-with-claude`
**Branch**: `feat/sdk-params-align`

## Summary

Recent CLI commits (`feat/cli-align-with-claude`) added several new `Config` fields
(`session_name`, `thinking_budget`, `max_budget_usd`, `extra_dirs`, `planning_mode`,
`permission_mode`, `append_system_prompt`) that were never exposed through the HTTP API
or SDKs. This PR bridges that gap.

## Files touched

- `src/http/mod.rs` — `CreateSessionRequest` and `RunRequest` structs: added new optional fields
- `src/http/handlers.rs` — `create_session` and `run_agent` handlers: wire new fields into runtime building; added `parse_planning_mode` / `parse_permission_mode` helpers
- `sdk/python/recursive_sdk/agent.py` — `Agent.create()` and `Agent.prompt()`: new keyword arguments
- `sdk/typescript/src/agent.ts` — `AgentOptions` interface: new typed fields; `Agent.create()` and `Agent.prompt()` send them to the server

## What was added

### HTTP API (`POST /sessions`, `POST /run`)

| Field | Type | Description |
|-------|------|-------------|
| `append_system_prompt` | `string?` | Append to default system prompt (ignored if `system_prompt` set) |
| `session_name` | `string?` | Display name (CreateSession only) |
| `max_steps` | `int?` | Per-session step cap (CreateSession only) |
| `planning_mode` | `"immediate"\|"plan_first"?` | Planning mode |
| `permission_mode` | `"default"\|"auto"\|"strict"\|"bypass"?` | Permission enforcement |
| `thinking_budget` | `int?` | Extended-thinking token budget (stored, not yet enforced) |
| `max_budget_usd` | `float?` | Max spend limit (stored, not yet enforced) |

### Python SDK
- `Agent.create()`: new kwargs `append_system_prompt`, `session_name`, `max_steps`, `planning_mode`, `thinking_budget`, `permission_mode`, `max_budget_usd`
- `Agent.prompt()`: new kwargs `append_system_prompt`, `planning_mode`, `thinking_budget`, `permission_mode`, `max_budget_usd`

### TypeScript SDK
- `AgentOptions` interface: new fields `appendSystemPrompt`, `sessionName`, `maxSteps`, `planningMode`, `thinkingBudget`, `permissionMode`, `maxBudgetUsd`
- `PromptOptions` now inherits everything from `AgentOptions` (removed redundant `maxSteps` duplication)
- `Agent.create()` and `Agent.prompt()` send all new options to the server

## Tests added

None — the new fields are thin pass-throughs; existing HTTP handler and runtime tests cover the wired paths.

## Notes

- `thinking_budget` and `max_budget_usd` are accepted and forwarded but enforcement is
  a separate milestone (same status as in the CLI per the commit message: "field wired, gate not yet enforced mid-run").
- `extra_dirs` (sandbox expansion) is intentionally **not** exposed through the HTTP API / SDK
  because it controls server-side filesystem access and should only be set at server startup via CLI.
- `append_system_prompt` takes precedence semantics: if `system_prompt` is also provided,
  `append_system_prompt` is silently ignored (server replaces the full prompt).
