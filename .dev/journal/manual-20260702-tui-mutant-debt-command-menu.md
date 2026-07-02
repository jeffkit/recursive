# Manual edit: tui-mutant-debt-command-menu

**Date**: 2026-07-02
**Goal**: Reduce the 31 missed mutants in `crates/recursive-tui/src/ui/command_menu.rs` listed in `.dev/mutant-debt-20260701.md`.

**Worktree**: `.worktrees/tui-mutant-debt-menu` (branch `tui-mutant-debt-menu`).

**Files touched**:
- `crates/recursive-tui/src/ui/command_menu.rs` — added two test modules.
- `.dev/mutant-debt-20260701.md` — marked `ui/command_menu.rs` done with unkillable residuals documented.

**Tests added** (48 new, in `tests` + `render_debt_tests` modules):
- Pure functions: `summary` (Builtin/Skill), `tab_completion_target` (single-exact-match kills the `1 =>` arm; multi-common-extends kills `>`->`<`), `tab_complete_names` (same), `popup_rect` (some-when-fits kills `-> None`; boundary y==popup_h kills `<`->`==`/`<=`; frame-overlap kills `-`->`+`; frame-just-below kills `-`->`/`; zero-candidates).
- `panel_height`: command-mode-with-skill (kills delete Command arm, `+`->`-`/`*` in builtin+skill sum, `+`->`-` in visible+2); visible-three (kills `+`->`*` in visible+2); atfile-mode (kills delete AtFile arm + `+`->`-`); history-search matches=3 (kills `+`->`-`/`*` in clamp+2); command-interact capped (kills `+`->`-` in MAX_VISIBLE+2 cap).
- Render (via `TestBackend` + buffer inspection): `render()` popup (in-mode renders, not-in-mode skips, selected-row Yellow highlight, skill argument-hint shown); `render_atfile()` popup (in-mode, skip, Cyan highlight); `render_panel` dispatch (Command/AtFile/HistorySearch arms) + selection highlights for each; `render_history_panel` truncation (61-char `…`, 60-char no-`…`) + LightGreen highlight; `render_permission_modal` (renders-when-pending, centered-x `/`, separator length, args truncation at 60/61 boundaries).

**Quality gates**: `cargo fmt`, `cargo clippy -p recursive-tui --all-targets -- -D warnings`, `cargo test -p recursive-tui --lib command_menu` (60 passed), scoped `tui-mutants.sh --jobs 4` on the file.

**Result**: 112 mutants → 107 caught, 3 missed + 1 timeout. The 31 listed missed were all killed. Residual 4 are genuinely unkillable (documented in the debt file):
- `86:13 +=`->`*=`: infinite-loop timeout, non-termination not assertable.
- `316:46 >`->`==`/`>=` (×2): popup width clamp (60) clips the `…`, orig/mutant buffers identical.
- `640:72 -`->`+`: separator overflows inner width by 2 chars that Paragraph clips, identical visible output.

**Notes**:
- The original debt list (31) under-counted: a full-file scan reveals 112 mutants. The 31 were the baseline survivors; my tests also killed newly-revealed survivors in `render` (180), `render_atfile` (239), `render_atfile_panel` (451/467), `render_command_panel` (422), `render_history_panel` (505/510), and `render_permission_modal` args truncation (630).
- `App::new().commands` differs from `CommandRegistry::default_set()` (more entries / skills), so tests that need deterministic match counts set `app.commands = CommandRegistry::default_set()` explicitly.
- All worktree commits use `git commit-tree` plumbing to avoid the IDE's `Co-authored-by` trailer injection.
