# Manual edit: goal-100-tool-rename

**Date**: 2026-06-04
**Goal**: Rename all tool spec name strings to Claude Code conventions, delete `apply_patch` and `list_dir`, add `GlobTool`, and add `is_deferred()` / `split_eager_deferred()` for eager/deferred partition.
**Files touched**:
- `src/tools/fs.rs` — deleted `ListDir` struct and impl
- `src/tools/apply_patch.rs` — deleted entire file
- `src/tools/glob.rs` — new `GlobTool` (walkdir-based glob)
- `src/tools/mod.rs` — removed `apply_patch`/`list_dir` exports, added `pub type SpecWithHint`, updated `split_eager_deferred()` signature, added `is_deferred()` overrides
- `src/cli/builder.rs` — swapped `ApplyPatch`/`ListDir` for `GlobTool`
- `src/tui/app/render.rs` — removed `apply_patch` branch, deleted `parse_apply_patch_input()`
- `src/tui/app/mod.rs` — removed `parse_apply_patch_input` re-export
- `src/tui/app/event_loop.rs` — removed `apply_patch` handler branch
- `src/tui/bash.rs` — `"run_shell"` → `"Bash"`
- `src/tui/app/commands.rs`, `src/tui/ui/transcript.rs`, `src/tui/keymap.rs`, `src/tui/commands.rs`, `src/tui/backend.rs`, `src/tui/skill_commands.rs` — updated all old tool names
- `src/permissions/mod.rs` — updated test fixtures; fixed wildcard tests (`"run_*"` no longer matches `"Bash"`)
- `src/permissions/auto_classifier.rs` — updated test fixtures
- `src/hooks/config.rs`, `src/hooks/mod.rs`, `src/hooks/external.rs` — updated all old tool names
- `src/llm/anthropic.rs`, `src/llm/openai.rs` — updated test fixtures
- `src/http/handlers.rs` — updated test fixtures
- `src/tools/docker_sandbox.rs`, `src/tools/e2b_provider.rs`, `src/tools/docker_provider.rs`, `src/tools/sub_agent.rs`, `src/tools/spawn_worker.rs` — updated names/comments
- `src/config_file.rs`, `src/mcp_server.rs`, `src/tool_set_provider.rs` — updated test fixtures
- `tests/smoke.rs`, `tests/integration.rs`, `tests/checkpoint_e2e.rs`, `tests/anthropic_smoke.rs`, `tests/tui_backend_smoke.rs`, `tests/incremental_writes.rs`, `tests/orphan_resume.rs`, `tests/resume_by_id.rs`, `tests/v050_integration.rs`, `tests/http.rs` — updated all tool name strings

**Tests added**: `GlobTool` unit tests in `src/tools/glob.rs`
**Notes**:
- `tests/mcp_integration.rs` intentionally left unchanged — all tests are `#[ignore]` and test an external MCP filesystem server (`npx @modelcontextprotocol/server-filesystem`) that uses its own `"read_file"`/`"write_file"` names; acceptance grep only covers `src/`.
- Added `pub type SpecWithHint = (ToolSpec, Option<String>)` to satisfy clippy `type_complexity` lint on `split_eager_deferred()`.
- Permissions wildcard tests updated: `"run_*"` now correctly matches `"run_background"` (kept its name) but not `"Bash"` (renamed from `"run_shell"`).
- Acceptance check: `grep -r '"read_file"\|"write_file"\|"run_shell"\|"str_replace"\|"search_files"\|"list_dir"\|"web_fetch"\|"todo_write"\|"sub_agent"\|"load_skill"\|"apply_patch"' src/` returns zero hits.
- All three quality gates pass: `cargo test --workspace`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt --all`.
