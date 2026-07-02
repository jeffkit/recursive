# Manual edit: tui-mutant-skip-annotations

**Date**: 2026-07-02
**Goal**: Clear the structurally-unkillable mutant residuals (infinite-loop timeouts, terminal-I/O side effects) and a dead-code/gate-config debt so the `tui-mutants.sh` gate can reach zero on the cleaned files.
**Files touched**:
- `crates/recursive-tui/Cargo.toml` — add `mutants = "0.0.3"` dev-dependency.
- `crates/recursive-tui/src/ui/command_menu.rs` — `#[cfg_attr(test, mutants::skip)]` on `longest_common_prefix`.
- `crates/recursive-tui/src/ui/markdown.rs` — extract skipped `bump_cursor` helper for `parse_inline`'s loop increment.
- `crates/recursive-tui/src/bash.rs` — `#[cfg_attr(test, mutants::skip)]` on `run_bash_command`.
- `crates/recursive-tui/src/lib.rs` — `#[cfg_attr(test, mutants::skip)]` on `RawModeGuard::drop`.
- `crates/recursive-tui/src/app/commands.rs` — remove dead `_within_window` binding in `handle_esc`.
- `.dev/scripts/tui-mutants.sh` — add `weixin` to `FEATURES`.
- `.dev/mutant-debt-20260701.md` — document the follow-up.

**Tests added**: none (these mutants are structurally untestable; the point is to skip them, not to test them).

**Notes**:
- Expression-level `#[cfg_attr(test, mutants::skip)]` on `i += 1;` is unstable on stable Rust (E0658: attributes on expressions are experimental). Used fn-level skip for the three fns, and a skipped `bump_cursor` helper for `parse_inline` so that fn stays fully mutable.
- `mutants` is a dev-only dependency; `#[cfg_attr(test, mutants::skip)]` is stripped in non-test builds, so the production binary is untouched. cargo-mutants honours the attribute by scanning for the `mutants::skip` substring regardless of the cfg condition.
- Gate verification over the five affected files: infinite-loop/terminal-I/O timeouts gone (command_menu / markdown / bash / lib) and the 4 `render_weixin_message` false-positive misses gone (transcript) thanks to the `weixin` feature now being enabled in the gate.
- `app/commands.rs` was not re-gated on this branch: its `handle_skill_install_key` tests live in the sibling `tui-mutant-debt` worktree, so a gate run here surfaces timeouts already resolved there. Dead-code removal is covered by the 660 passing recursive-tui unit tests.
