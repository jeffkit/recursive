# Manual edit: inline-tui-logo

**Date**: 2026-06-03
**Goal**: Replace full-screen TUI splash with inline viewport + ASCII logo startup banner
**Files touched**:
- `src/tui/mod.rs` — rewrote `run()`: removed `EnterAlternateScreen`, added `Viewport::Inline(22)`, added `print_startup_banner()`
- `src/tui/model.rs` — removed `AppScreen::Splash` variant (only `Chat` remains)
- `src/tui/app/mod.rs` — renamed `splash_start: Instant` → `start_time: Instant`
- `src/tui/app/state.rs` — changed initial `screen` to `AppScreen::Chat`, updated tests
- `src/tui/commands.rs` — updated `cmd_status` to use `app.start_time`
- `src/tui/ui/mod.rs` — removed `pub mod splash;` and the `Splash` match arm
- `src/tui/ui/splash.rs` — deleted

**Tests added**: Updated existing tests (`app_new_starts_in_splash_screen` → `app_new_starts_in_chat_screen`, removed `splash_auto_transitions_after_elapsed`)

**Notes**:
- Before the TUI starts, `print_startup_banner()` prints the Cyan bold RECURSIVE ASCII logo, version, and up to 5 recent sessions to stdout — this stays in the terminal's scrollback buffer.
- The TUI no longer uses `EnterAlternateScreen`; `Viewport::Inline(22)` occupies a fixed region at the bottom of the main terminal buffer. Previous output (banner, history) remains visible above.
- Mouse capture removed since it would interfere with the user scrolling the terminal to see history above the viewport.
- `cargo test --features tui` and `cargo clippy --all-targets --features tui -- -D warnings` both clean.
