# Manual edit: tui-mutant-debt-markdown

**Date**: 2026-07-02
**Goal**: Reduce the 28 missed mutants in `crates/recursive-tui/src/ui/markdown.rs` listed in `.dev/mutant-debt-20260701.md`.

**Worktree**: `.worktrees/tui-mutant-debt-md` (branch `tui-mutant-debt-md`).

**Files touched**:
- `crates/recursive-tui/src/ui/markdown.rs` — added `debt_tests` module (30 tests).
- `.dev/mutant-debt-20260701.md` — marked `ui/markdown.rs` done with unkillable residuals documented.

**Tests added** (30, in `debt_tests`):
- Lazy init: `syntax_set`/`theme_set` non-empty defaults.
- `is_table_line`, `parse_table_rows` (dash + alignment separators).
- `render_table`: width cap (1-col + 3-col), header-only no divider, top border width/separators.
- `render_markdown`: emphasis, ordered list, heading + heading/body split, nested list pop, soft/hard break, two paragraphs, list-then-paragraph (TagEnd::Item), H1/H2/H3 colours, bold/heading style pop.
- `syntect_color_to_ratatui`, `strip_heading`, `truncate_to_visual_width`.
- `parse_inline`/`is_double`: bold-at-start, `a__b`, `*a**b*`, `_*x*_`, `a**b*`, `a*b*`.

**Quality gates**: `cargo fmt`, `cargo clippy -p recursive-tui --all-targets -- -D warnings`, `cargo test -p recursive-tui --lib markdown` (60 passed), scoped `tui-mutants.sh --jobs 4` on the file.

**Result**: 219 mutants → 204 caught, 12 missed + 2 timeout. All 28 debt-listed mutants killed. Residual 14 are genuinely unkillable (documented in debt file): render_table width-cap off-by-one guards (×4), no-op Paragraph/Table* Start arms (×4), style-stack pop `>`→`>=` (×2), ncols>0 (×1), parse_inline loop non-termination (×2), is_double next-bound (×1).

**Notes**:
- Full-file scan reveals 219 mutants vs debt baseline 28; tests also killed newly-revealed survivors in `make_border_line`, `TagEnd::Paragraph`, `Event::SoftBreak`/`HardBreak`, heading level arms, etc.
- Nested-list TagEnd::List test must assert `!text.contains("3.")` — a bare `contains('•')` also matches the first outer item.
- All worktree commits use `git commit-tree` plumbing to avoid IDE `Co-authored-by` injection.
