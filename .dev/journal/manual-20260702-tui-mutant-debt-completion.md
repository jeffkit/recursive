# Manual edit: tui-mutant-debt-completion

**Date**: 2026-07-02
**Goal**: Reduce the 17 missed mutants in `crates/recursive-tui/src/completion.rs` listed in `.dev/mutant-debt-20260701.md`.

**Worktree**: `.worktrees/tui-mutant-debt-comp` (branch `tui-mutant-debt-comp`).

**Files touched**:
- `crates/recursive-tui/src/completion.rs` — added `debt_tests` module (8 tests).
- `.dev/mutant-debt-20260701.md` — marked `completion.rs` done (0 unkillable).

**Tests added** (8, in `debt_tests`, using `tempfile::TempDir`):
- `default_offline_tool_catalog_has_six_named_entries`: kills all three 25:5 mutants (vec![], single-empty, single-xyzzy).
- `glob_workspace_files_finds_cargo_toml_in_cwd`: kills 86:5 (-> vec![]).
- `collect_files_populates_out_vec`: kills 118:5 (-> ()).
- `collect_files_walks_four_levels_deep`: kills 118:14 `>`->`==`/`<`/`>=` (depth cut-off) and 133:46 `+`->`*` (depth never grows -> walks too deep).
- `collect_files_skips_hidden_target_and_node_modules`: kills 129:50/74 `==`->`!=` and 129:62 `||`->`&&`.
- `collect_files_nonempty_query_matches_substring`: kills 139:34 `||`->`&&`.
- `collect_files_caps_at_four_times_max_suggestions`: kills 140:30 `<`->`==`/`>`/`<=` and 140:55 `*`->`/`.

**Quality gates**: `cargo fmt`, `cargo clippy -p recursive-tui --all-targets -- -D warnings`, `cargo test -p recursive-tui --lib completion` (9 passed), scoped `tui-mutants.sh --jobs 4` on the file.

**Result**: 28 mutants → 28 caught, **0 missed, 0 timeout**. All 17 debt-listed mutants killed; no unkillable residuals.

**Notes**:
- `depth > 3` walks while depth <= 3, so orig collects files at paths up to 3 components deep (e.g. `d1/d2/d3/l3.txt`) but does NOT recurse into a depth-4 dir — the depth-4 file is never collected. Initial test asserting the depth-4 file was present failed; corrected to assert depth-3 collected + depth-4/5 absent.
- `glob_workspace_files` is tested against the real CWD (the worktree always has a root `Cargo.toml`); `collect_files` is tested with `TempDir` for deterministic structure.
- All worktree commits use `git commit-tree` plumbing to avoid IDE `Co-authored-by` injection.
