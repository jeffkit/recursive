# Manual edit: tui-suppress-tracing-in-alternate-screen

**Date**: 2026-06-03
**Goal**: `tracing` INFO lines were leaking into the TUI's alternate
screen — the user saw lines like
`2026-06-03T03:36:49Z INFO agent.turn: finished steps=2 …` rendered
right next to the input box because `tracing-subscriber` was
initialised in `main.rs` *before* the TUI entered the alternate
screen, and stderr writes (same fd as the alternate screen) landed
inside the TUI surface.

**Fix**: a `TUI_QUIET` atomic flag plus a custom `StderrOrNull`
writer that drops every byte while the flag is set. `tui::run()`
takes an RAII guard (`suppress_tracing_for_tui`) at the very top of
the function so the flag is restored on every exit path — including
panics — letting the user still see panic messages on stderr after
`LeaveAlternateScreen`.

**Files touched**:
- `src/logging.rs` (new) — `TUI_QUIET` atomic, `set_tui_quiet` /
  `is_tui_quiet`, `TuiQuietGuard` RAII type, `suppress_tracing_for_tui`,
  `StderrOrNull` (impls `io::Write`), `StderrOrNullMaker` (impls
  `tracing_subscriber::fmt::MakeWriter`). Three unit tests:
  `stderr_or_null_drops_bytes_when_quiet`,
  `guard_restores_quiet_state_on_drop`,
  `guard_restores_even_if_another_guard_already_dropped`
  (the last one documents the single-shot, non-reference-counted
  guard behaviour).
- `src/lib.rs` — `pub mod logging;` between `llm` and `mcp`.
- `src/main.rs::init_logging` — `with_writer(StderrOrNullMaker)`
  replaces the previous `with_writer(std::io::stderr)`.
- `src/tui/mod.rs::run` — `let _quiet_guard =
  crate::logging::suppress_tracing_for_tui();` placed at the very
  top of the function so the flag is set before `enable_raw_mode`
  / `EnterAlternateScreen` run (and the guard is alive for the
  rest of the function — Drop fires even on `?` early-returns).

**Quality gates**:
- `cargo test --workspace` — clean.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — clean.

**Notes**:
- The guard is **single-shot, not reference-counted**. Nesting
  `suppress_tracing_for_tui()` calls is safe in practice (the TUI
  only takes one), but the inner drop will prematurely clear the
  flag for the outer scope. We use one guard per scope and the
  existing test `guard_restores_even_if_another_guard_already_dropped`
  documents this so any future refactor that tries to make it
  re-entrant will trip the assertion and force a rethink.
- This does **not** solve the raw-mode / alternate-screen cleanup
  problem on panic. The companion `TerminalGuard` (already on
  main) handles the alternate-screen teardown, but a separate
  panic-during-init race still exists; flag if you hit it.
- Anything that needs to surface to the user during a TUI session
  (hook progress, the spinner verb, etc.) should flow through the
  TUI's own event sink as a `UiEvent`, not the global subscriber.
  `logging.rs` documents this so future contributors don't add
  stray `tracing::info!` lines that get silently swallowed.
