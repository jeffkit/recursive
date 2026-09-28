# Manual — issue #42: unknown-model cost must not fake $0.0000

**Date**: 2026-09-28
**Goal**: When a model has no pricing entry, cost reporting should signal "unknown" instead of silently reporting `$0.0000`.

## Files touched
- `src/cost.rs` — `.meta.json` `cost_usd` is now `null` when `cost_usd()` returns `None` (was coerced to `0.0`).
- `crates/recursive-cli/src/cli/output.rs` — new `format_cost_line(Option<f64>, model)`; CLI prints `cost: unknown (no pricing for <model>)`.
- `tests/issue42_unknown_cost.rs` — integration test for the null `cost_usd` in `.meta.json`.

## Tests added
- `src/cost.rs::tests::test_meta_cost_usd_null_for_unknown_model`
- `output.rs::tests::format_cost_line_priced_model_shows_amount` / `format_cost_line_unknown_model_shows_unknown`
- `tests/issue42_unknown_cost.rs`

## Notes
Docs only this pass: CHANGELOG Unreleased entry + this journal. No source changes, no commit.
