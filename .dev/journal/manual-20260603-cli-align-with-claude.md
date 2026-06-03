# Manual edit: cli-align-with-claude

**Date**: 2026-06-03
**Goal**: Align Recursive CLI parameters with Claude Code's CLI, adding high/medium priority options
**Files touched**:
- `src/main.rs` — added 5 new CLI parameters, `effective_plan_first` resolution, `continue_session` routing
- `src/config.rs` — added `thinking_budget: Option<u32>` and `session_name: Option<String>` fields
- `src/session.rs` — added `name: Option<String>` to `SessionMeta` and `SessionWriter`; added `set_name()` method; updated `finish()` to persist name
- `tests/v050_integration.rs`, `tests/agui_e2e.rs`, `tests/agent_team_integration.rs`, `tests/http.rs` — added new Config fields
- `src/multi.rs`, `src/tools/team_manage.rs` — added new Config fields

**New CLI parameters added**:
| Parameter | Description |
|-----------|-------------|
| `-c/--continue` | Continue the most recent conversation (equivalent to `recursive resume` with no args) |
| `-n/--name` | Set a display name for this session (shown in `sessions list` and `/resume` picker) |
| `--effort low\|normal\|high` | Reasoning effort: maps to `thinking_budget` (0/default/16000) |
| `--append-system-prompt <text>` | Append text to the default system prompt without replacing it |
| `--permission-mode default\|plan\|auto` | Permission mode: `plan` = PlanFirst, `auto` = headless (auto-approve all) |

**Backward compatibility**:
- `--plan-first` is preserved as an alias for `--permission-mode=plan`
- All new `Config` fields default to `None` so existing code paths are unaffected
- `SessionMeta.name` and `SessionWriter.name` use `skip_serializing_if = "Option::is_none"` for clean round-trips

**Tests added**: none (existing session tests verify the new `name` field round-trips correctly)
**Notes**: `--effort` stores the budget in `Config.thinking_budget` for future use by the Anthropic provider; the field exists but is not yet consumed by the LLM layer (that's a separate task).
