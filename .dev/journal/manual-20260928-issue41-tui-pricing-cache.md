# Manual edit: issue-41 TUI per-frame pricing IO

**Date**: 2026-09-28
**Goal**: Issue #41 — status-bar pricing resolves the provider catalog (disk IO + TOML/JSON parsing) once per model, not once per render frame
**Branch**: (working tree, uncommitted)

## Files touched

- `crates/recursive-tui/src/app/mod.rs` — `App` gains `pricing_cache` /
  `pricing_lookups` fields
- `crates/recursive-tui/src/app/state.rs` — `App::pricing_for_model` (cache +
  lookup counter), `reset_pricing_lookup_count` / `pricing_lookup_count`
- `crates/recursive-tui/src/cost.rs` — extract `estimate_cost_with_pricing`
  for already-resolved pricing entries
- `crates/recursive-tui/src/ui/modal.rs` — cost modal uses the App cache
- `crates/recursive-tui/src/ui/status.rs` — `build_line` uses the App cache
- new test `crates/recursive-tui/tests/status_pricing_cache.rs`

## Tests added

- `build_line_resolves_pricing_once_per_model_not_per_frame` — three frames
  with the same model resolve pricing at most once (via `pricing_lookup_count`)

## Notes

- Render hot path no longer calls `llm::pricing_for` directly; cache is
  single-entry (current model).
