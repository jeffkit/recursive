# Manual edit: unify-system-prompt

**Date**: 2026-06-30
**Goal**: Collapse the scattered system-prompt assembly into a single common
entry point and decouple sub-agent (coordinator prompt + `Agent` tool) from
the CLI `build_runtime`, so every agent-loop channel (CLI run/do, CLI loop,
HTTP API, TUI) builds the prompt and registers sub-agent identically.
Channels now only differ in channel-specific inputs (e.g. where
`append_system_prompt` comes from), which they fold into the `base` before
calling the common assembler.
**Files touched**:
- `src/config.rs` — added `pub subagent_enabled: bool` field, read from
  `RECURSIVE_SUBAGENT_ENABLED` / `RECURSIVE_TEAM_ENABLED` in `Config::from_env`
  (next to `subagent_max_depth`).
- `src/system_prompt.rs` (new) — `assemble_system_prompt(base, workspace,
  skills, sub_agent_enabled)`; the single common assembly entry point.
  Order: project context (AGENTS.md + CLAUDE.md) → base → skill_index →
  (coordinator workflow + sub_agent note, only when enabled). Unit tests
  cover no-files/only-AGENTS/only-CLAUDE/both × skills × subagent toggle.
- `src/multi.rs` — added `register_subagent_if_enabled(tools, config,
  provider)`; the channel-agnostic hook that registers the unified `Agent`
  tool when `config.subagent_enabled`. Re-exports added in `src/lib.rs`.
- `src/lib.rs` — `pub mod system_prompt;`, re-export `assemble_system_prompt`
  and `register_subagent_if_enabled`.
- `crates/recursive-cli/src/cli/builder.rs` — `build_runtime` slimmed: removed
  inline `AgentTool` registration, `coordinator_suffix`, `skill_index` match,
  and the sub_agent note block. Now calls `register_subagent_if_enabled` +
  `assemble_system_prompt`. The CLI-run-only goal-based skill *body*
  injection (`skills_for_injection`) stays as a channel-specific suffix
  after assembly. `discover_loaded_skills` made `pub(crate)`. Dropped unused
  imports (`AgentTool`, `AgentDefinitions`, `coordinator_system_prompt`,
  `skill_index`).
- `crates/recursive-cli/src/main.rs` — `run_loop` now discovers skills,
  calls `register_subagent_if_enabled`, and builds the prompt via
  `assemble_system_prompt` (previously used `config.system_prompt` directly
  with no skill index / no sub-agent / no coordinator — a latent gap now
  fixed). HTTP `AppState` construction builds the provider first, then
  registers sub-agent on `tools` before deriving `tool_infos` (so
  `/tools/list` advertises `Agent` when enabled), and uses
  `discover_loaded_skills` for skills. Removed the `prepend_project_context`
  pre-bake at the old `:546` — each channel assembles itself now.
- `src/http/handlers.rs` — all 4 agent-building handlers (`run_agent`,
  `create_session`, fork-session, `agui_run`) replaced their hand-written
  `skill_index` append with a single `assemble_system_prompt` call. HTTP
  `body.system_prompt = Some(...)` now also gets project context prepended
  (previously lost it).
- `crates/recursive-tui/src/runtime_builder.rs` — both `build_runtime` and
  `build_runtime_with_skill_tx` call `register_subagent_if_enabled` +
  `assemble_system_prompt`. Deleted the `tui_system_prompt` helper, its two
  tests, and the now-unused `make_skill` test helper + imports.
- `docs/architecture/layer0-injection.md` — rewritten to describe
  `assemble_system_prompt` as the single entry point and the new injection
  order; "Adding a New Layer 0 Source" now points at the one place to edit.
**Tests added**:
- `src/system_prompt.rs`: 5 tests (no-files/no-skills/no-subagent returns
  base; prepends AGENTS+CLAUDE sections; skill_index when skills present;
  subagent suffix only when enabled; ordering invariant).
- Removed `tui_system_prompt_appends_skill_index` and
  `tui_system_prompt_unchanged_when_no_skills` (helper deleted).
**Notes**:
- Deliberately did NOT move project-context reading into `Config::from_env`:
  many tests call `from_env()` with cwd = repo root where AGENTS.md /
  CLAUDE.md really exist; baking ~16 KB into `config.system_prompt` there
  would make tests env-dependent. The common assembly lives in
  `assemble_system_prompt`, called only at agent-construction (production
  paths), so tests that never construct a runtime stay hermetic. HTTP
  handler tests construct `AppState` from `from_env()` (workspace = repo
  root) and now go through `assemble_system_prompt`, which reads the real
  AGENTS.md/CLAUDE.md; their assertions are on HTTP status/behavior, not
  prompt content, so they still pass.
- MCP server mode (`run_mcp_server_stdio`) is a pure tool dispatcher with
  no agent loop and no system prompt — intentionally not changed.
- `coordinator::filter_registry` (`RECURSIVE_COORDINATOR_MODE`) is a
  separate concern from sub-agent and left untouched.
- Behavior changes (intended, user-confirmed): `recursive loop`, HTTP, and
  TUI now register the `Agent` tool + inject the coordinator prompt when
  `RECURSIVE_SUBAGENT_ENABLED=1` (or `RECURSIVE_TEAM_ENABLED=1`); previously
  only CLI `run`/`do` did. HTTP `body.system_prompt` now also gets project
  context prepended.
- `gitnexus_impact` on `load_project_context`: LOW. Quality gates:
  `cargo fmt --all` clean; `cargo clippy --all-targets --all-features
  -- -D warnings` clean; `cargo test --workspace --all-features` 1104
  passed, 1 pre-existing flaky failure
  (`tools::team_delete::tests::delete_existing_team`, unrelated — passes
  in isolation).
