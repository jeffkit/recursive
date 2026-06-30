# Manual edit: project-context-both-files

**Date**: 2026-06-30
**Goal**: Make every agent launch path load both `AGENTS.md` and `CLAUDE.md`
into the system prompt. Previously only the CLI `run` path injected
`AGENTS.md` (in `cli/builder.rs`); MCP, HTTP API, and TUI got neither file,
and `CLAUDE.md` was never loaded anywhere.
**Files touched**:
- `src/config.rs` — rewrote `load_project_context` to merge `AGENTS.md` +
  `CLAUDE.md` (each capped at 16 KB via new `load_capped_md` helper, emitted
  under `## AGENTS.md` / `## CLAUDE.md` sub-headers). Added
  `prepend_project_context(base, workspace)` that wraps the merged block as
  `# Project context\n\n{ctx}\n\n---\n\n{base}`.
- `crates/recursive-cli/src/main.rs` — after `--system-prompt` /
  `--append-system-prompt` overrides, call `prepend_project_context` on
  `config.system_prompt`. This covers CLI `run` / `loop` / `mcp` and the HTTP
  API (all share the one `config` built in `run`).
- `crates/recursive-cli/src/cli/builder.rs` — removed the now-redundant
  `load_project_context` injection (would double-inject). Simplified the
  system-prompt match to layer on `skill_index` + coordinator suffix only.
  Dropped the unused import.
- `crates/recursive-tui/src/runtime_builder.rs` — TUI is a separate binary
  that calls `Config::from_env()` itself, so inject
  `prepend_project_context` at both `build_runtime` and
  `build_runtime_with_skill_tx` before `tui_system_prompt`.
- `docs/architecture/layer0-injection.md` — updated to reflect both files,
  the new helper, and the moved injection points.
**Tests added**:
- `config::tests::test_d_load_project_context_includes_claude_md`
- `config::tests::test_e_load_project_context_merges_both_files`
- `config::tests::test_prepend_project_context_wraps_base`
- `config::tests::test_prepend_project_context_no_op_when_absent`
Existing `test_a/b/c_load_project_context_*` still pass (assertions are
`contains(...)` / `None`, satisfied by the new format).
**Notes**:
- Deliberately did NOT inject inside `Config::from_env()` even though that
  looks like the cleanest single choke point: many tests call
  `Config::from_env()` with cwd = repo root, where `AGENTS.md` / `CLAUDE.md`
  really exist. Injecting there would bake ~16 KB of repo-specific content
  into `config.system_prompt` for every test, making them env-dependent and
  flaky. Injecting at the launch entry points (main.rs / TUI builder) keeps
  tests hermetic.
- HTTP handler tests build `AppState` directly from `Config::from_env()`
  without going through `main.rs`, so they still see no project context —
  this matches pre-existing behavior and their assertions don't reference
  project context.
- `gitnexus_impact` on `load_project_context`: LOW risk (only 3 test callers
  + `build_runtime`). `gitnexus_detect_changes`: LOW, 0 affected processes.
- Quality gates: `cargo fmt --all` clean; `cargo clippy --all-targets
  --all-features -- -D warnings` clean; `cargo test --workspace --all-features`
  1099 passed, 1 pre-existing flaky failure
  (`tools::team_delete::tests::delete_existing_team`, unrelated — passes in
  isolation, touches team file paths I did not modify).
