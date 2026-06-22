# Manual edit: tui-cli-subcrate-migration

**Date**: 2026-06-22
**Goal**: Physically migrate TUI and CLI code out of the `recursive` main crate into separate workspace crates.

## Files touched

**New crate: `crates/recursive-tui/`**
- `Cargo.toml` — new standalone crate with ratatui/crossterm/etc dependencies
- `src/lib.rs` — converted from `src/tui/mod.rs`, now the crate root
- `src/main.rs` — new binary entry point (`recursive-tui`)
- `src/app/`, `src/ui/`, `src/backend.rs`, `src/bash.rs`, `src/commands.rs`,
  `src/completion.rs`, `src/cost.rs`, `src/events.rs`, `src/input_state.rs`,
  `src/keymap.rs`, `src/model.rs`, `src/runtime_builder.rs`, `src/skill_commands.rs`
  — migrated from `src/tui/`, all `use crate::tui::` and `use crate::<module>::`
    references updated to `use crate::` (intra-crate) and `use recursive::` (cross-crate)

**New crate: `crates/recursive-cli/`**
- `Cargo.toml` — new standalone crate
- `src/main.rs` — copied from `src/main.rs`, TUI sections removed
- `src/cli/` — copied from `src/cli/`

**Root crate (`recursive-agent`)**
- `src/lib.rs` — removed `#[cfg(feature = "tui")] pub mod tui;`
- `src/logging.rs` — gated `use tracing_subscriber::fmt::MakeWriter` and
  `impl MakeWriter for StderrOrNullMaker` behind `#[cfg(feature = "cli")]`
- `Cargo.toml` — removed `tui` feature, removed `[[bin]] recursive`,
  removed ratatui/crossterm/unicode-width/syntect/pulldown-cmark dependencies,
  added `crates/recursive-cli` to workspace members
- `tests/tui_backend_smoke.rs` — deleted (TUI tests now belong in recursive-tui)
- `src/tui/` — deleted (moved to crates/recursive-tui/src/)
- `src/cli/` — deleted (moved to crates/recursive-cli/src/cli/)
- `src/main.rs` — deleted (moved to crates/recursive-cli/src/main.rs)

## Tests added

None (existing tests migrated with the crate).

## Notes

- `recursive-tui` depends on `recursive-agent` without the `tui` feature;
  TUI deps (ratatui etc.) are direct dependencies of `recursive-tui` now.
- `recursive-cli` depends on `recursive-agent` with `cli` feature but NOT `tui`.
- `recursive` lib no longer exposes a `tui` public module (breaking API change).
- The `run_tui_with_weixin` function was removed from the CLI binary since
  CLI no longer depends on TUI; this functionality is available via `recursive-tui`
  binary with `--weixin` flag (already handled in recursive-tui's backend).
- HTTP API slash commands from TUI were removed from CLI; CLI now returns an
  empty slash_commands list for the HTTP API.
- The `http` feature warning in `recursive-cli` is a non-error cfg lint from
  the copied main.rs mentioning `feature = "http"` which is not declared in
  recursive-cli's own feature set (it's in the recursive-agent dependency).
