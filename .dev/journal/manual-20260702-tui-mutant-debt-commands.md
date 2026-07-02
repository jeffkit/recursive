# Manual edit: tui-mutant-debt commands.rs

**Date**: 2026-07-02
**Goal**: Kill the 108 missed mutants in `crates/recursive-tui/src/app/commands.rs` from the 2026-07-01 whole-crate mutation baseline, by adding targeted tests in a dedicated `tui-mutant-debt` worktree.
**Files touched**:
- `crates/recursive-tui/src/app/commands.rs` — added 8 test batches (147 `#[test]`s total in the file) across new modules: `picker_tests`, `handle_key_tests`, `atfile_debt_tests`, `command_panel_tests`, `command_menu_tests`, `skill_install_tests` (feature-gated), plus the earlier `history_search_tests`.
- `crates/recursive-tui/src/backend.rs` — removed `#[mutants::skip]` from `weixin_final_text` (see below).
- `.dev/mutant-debt-20260701.md` — marked `app/commands.rs` done (108 → 3).
**Tests added**: ~80 new tests this session (history-search 17, picker 16, modal 10, handle_key/esc/ctrl_c 9, atfile 4, command_panel 12, command_menu 3, skill_install 18).
**Notes**:
- Final scoped `tui-mutants.sh --jobs 6 crates/recursive-tui/src/app/commands.rs`: **3 missed, 294 caught, 17 unviable, 5 timeout** (down from 108 missed). 105/108 killed.
- 3 accepted unkillable mutants (behavior-equivalent / dead code), documented in `.dev/mutant-debt-20260701.md`:
  - `215` `should_walk_history_down() guard→true` (history_next no-op when not walking)
  - `279` handle_esc `_within_window` unused (dead code)
  - `1445:27` modal_scroll `>`→`>=` (boundary idempotent)
- **Pre-existing latent bug fixed**: `backend.rs::weixin_final_text` carried `#[mutants::skip]` without `mutants` declared as a dependency. The attribute is only injected by cargo-mutants, so `cargo clippy --all-features` (weixin on) failed with E0433 "cannot find crate `mutants`" — breaking the mandated `--all-features` clippy gate that the self-improve flow runs (`self-improve.flow.js:112,329`). Merged in 8067b63. Removed the attribute: weixin-off → fn cfg'd out (no mutants, as intended); weixin-on → live body with killable mutants. Both `cargo clippy --workspace --all-targets --all-features -- -D warnings` and the default-feature clippy now pass.
- Commits in this worktree use `git commit-tree` plumbing to bypass the Cursor IDE's automatic `--trailer Co-authored-by` injection on `git commit` (per the no-Co-authored-by rule). `main`'s history still carries trailers from earlier in the session (user chose to leave main as-is).
- All gates run: `cargo fmt --check` clean, `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean, `cargo test --workspace` (running at journal time).
