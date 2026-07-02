# Manual edit: tui-mutant-debt-transcript

**Date**: 2026-07-02
**Goal**: Reduce the 2 missed mutants in `crates/recursive-tui/src/ui/transcript.rs`.

**Worktree**: `.worktrees/tui-mutant-debt-tr` (branch `tui-mutant-debt-tr`).

**Files touched**: `crates/recursive-tui/src/ui/transcript.rs` (20+ tests in `tests`), `.dev/mutant-debt-20260701.md`.

**Tests added** (full-file cleanup, 76 mutants total):
- `wrap_lines_to_width_emits_oversized_glyph_on_one_row`: kills 84:44 `>`->`>=` (the `cur_w > 0` guard, not the width compare — needs a wide glyph on a too-narrow row).
- `render_plan_mode_request_pending_shows_request_ui`: kills 638:5 (-> vec).
- `render_blocks_separates_with_blank_line`: kills 29:14 `>`->`>=`/`==`/`<` (uses two Assistant blocks so the separator isn't masked by User trailing blanks).
- `format_size_*` (×4): kills all 10 format_size mutants (425:5, 425:14, 427:21, 427:28).
- `plan_args_preview_*` (×6): kills all 10 plan_args_preview mutants (588:5, 596:54, 613:28, 614:51).
- `render_error_emits_text_line`: kills 472:5.
- `render_plan_proposal_emits_header`: kills 494:5.
- `render_tool_call_failure_uses_error_color_for_args`: kills 332:9.
- `render_tool_call_shows_size_row_when_output_nonempty`: kills 383:16.
- `render_tool_call_six_lines_fully_visible_unexpanded`: kills 396:57 and 407:32.
- `#[cfg(feature="weixin")] render_weixin_message_*` (×2): cover 143:5/165:47 under `--features weixin`.

**Result**: 76 mutants → 72 caught, 4 missed. 4 unkillable: `143:5`/`165:47` weixin mutants (default-feature gate has `weixin` OFF, so the cfg-gated code doesn't compile and the mutant never applies — a cargo-mutants false positive; the `#[cfg(feature="weixin")]` tests kill them under `--features weixin`).

**Gates**: cargo test (38 default + 2 weixin), clippy --all-features clean, scoped tui-mutants. Commits via `git commit-tree`.
