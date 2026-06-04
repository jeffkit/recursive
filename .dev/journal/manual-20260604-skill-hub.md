# Manual edit: skill-hub

**Date**: 2026-06-04
**Goal**: Implement `find_skills` (local search) and `install_skill` (remote install with TUI confirmation) tools for the Recursive agent.
**Files touched**:
- `Cargo.toml` — added `zip` optional dep, `skill-hub` feature
- `src/tools/find_skills.rs` — new tool: local skill directory search with scoring
- `src/tools/install_skill.rs` — new tool: skillhub.cn REST search, zip download, TUI-gated install
- `src/tools/mod.rs` — registered `FindSkills` / `InstallSkill` under `#[cfg(feature = "skill-hub")]`
- `src/tui/events.rs` — added `SkillInstallEvent`, `SkillSearchRequest`, `SkillFilesRequest`, `SkillSearchResult`, `SkillZipFile`
- `src/tui/ui/modal.rs` — added `Modal::SkillInstall` variant, `SkillInstallState`, `render_skill_install()` (3-stage interactive modal)
- `src/tui/app/mod.rs` — added `pending_skill_install` field
- `src/tui/app/state.rs` — added `handle_skill_search_request` / `handle_skill_files_request` methods
- `src/tui/app/commands.rs` — added `handle_skill_install_key` for 3-stage modal navigation
- `src/tui/backend.rs` — added `skill_install_rx` channel to `Backend`
- `src/tui/runtime_builder.rs` — added `build_runtime_for_tui()` / `build_runtime_with_skill_tx()`
- `src/tui/mod.rs` — wired `skill_install_rx` into the main `tokio::select!` event loop
- `.dev/goals/05-skill-discovery-install.md` — goal document

**Tests added**:
- `src/tools/find_skills.rs` — unit tests for scoring and empty-query fallback
- `src/tools/install_skill.rs` — unit tests for URL building, zip parsing, path sanitization

**Notes**:
- `install_skill` is only available in TUI mode; the `skill-hub` feature gate keeps it out of headless/CLI builds.
- The 3-stage modal flow: search-results → file-tree → file-preview, with oneshot channels blocking the tool until the user confirms/cancels.
- `tokio::select!` does not support `#[cfg]` on arms; worked around by always declaring `skill_install_rx` (dummy channel when feature is off).
- All Clippy lints fixed: `sort_by_key`, `split_once`, `items_after_test_module`, unused imports/variables.
- 1119 tests pass; `cargo clippy -D warnings` and `cargo fmt` clean.
