# Manual edit: tui-mutant-debt-app-render

**Date**: 2026-07-02
**Goal**: Reduce the 3 missed mutants in `crates/recursive-tui/src/app/render.rs`.

**Worktree**: `.worktrees/tui-mutant-debt-render` (branch `tui-mutant-debt-render`).

**Files touched**: `crates/recursive-tui/src/app/render.rs` (5 tests in `tests`), `.dev/mutant-debt-20260701.md`.

**Tests added** (5):
- `blocks_from_messages_emits_reasoning_block_when_non_empty`: kills 42:24 delete `!`.
- `clamp_returns_input_unchanged_at_exact_max`: kills 119:26 `<=`->`>`.
- `extract_write_file_path_from_result_finds_path_after_to`: kills 223:38 `+`->`-`.
- `parse_v4a_patch_splits_multiple_hunks`: kills 171:16 delete `!` (hunk split guard).
- `parse_v4a_patch_bare_anchor_omits_empty_context`: kills 177:16 delete `!` (bare `@@` anchor).

**Result**: 31 mutants → 26 caught, 3 unviable, 2 missed. 2 unkillable: `161:13`/`162:13` `||`->`&&` (patch-marker lines never match hunk prefixes, so the skip guard is behavior-neutral).

**Gates**: cargo test (5 passed), clippy clean, scoped tui-mutants. Commits via `git commit-tree`.
