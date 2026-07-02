# Manual edit: tui-mutant-debt-skill-commands

**Date**: 2026-07-02
**Goal**: Reduce the 3 missed mutants in `crates/recursive-tui/src/skill_commands.rs`.

**Worktree**: `.worktrees/tui-mutant-debt-sk` (branch `tui-mutant-debt-sk`).

**Files touched**: `crates/recursive-tui/src/skill_commands.rs` (2 tests in `tests`), `.dev/mutant-debt-20260701.md`.

**Tests added** (2):
- `search_paths_includes_workspace_skill_dirs`: kills both 120:9 mutants (-> vec![] and -> vec![Default::default()]).
- `parse_inline_list_unclosed_bracket_treated_as_single`: kills 384:27 `&&`->`||`.

**Result**: 39 mutants → 34 caught, 5 unviable, **0 missed**. All 3 debt-listed killed.

**Gates**: cargo test (2 passed), clippy clean, scoped tui-mutants. Commits via `git commit-tree`.
