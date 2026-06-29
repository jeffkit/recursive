# Manual edit: tui-test-harness stage 1 (in-process harness)

**Date**: 2026-06-29
**Goal**: Establish the observation loop for AI-driven TUI testing — an
in-process harness that drives `App` with key events + `UiEvent`s and
renders to an offscreen `ratatui::Buffer` via `TestBackend`. This is the
"AI's eyes" layer (stage 1 of 5) that makes targeted, verifiable TUI
test authoring possible.

**Files touched**:
- `crates/recursive-tui/src/harness.rs` (new) — `Harness` + `Screen`
- `crates/recursive-tui/src/lib.rs` (register `#[cfg(test)] pub mod harness`)
- `crates/recursive-tui/Cargo.toml` (add `recursive` test-utils dev-dep so
  `cargo test -p recursive-tui` is self-contained, no CLI flag needed)

**What the harness provides**:
- `Harness::new()` / `with_size(w,h)` — owns an `App`, no terminal, no backend.
- Input: `type_key` / `type_char` / `type_str` / `enter` / `ctrl` / `submit`.
  Dispatches through `keymap::dispatch` and queues the emitted `UserAction`s
  (what the UI would have sent to the backend worker).
- Events: `pump(UiEvent)` / `pump_many` — the in-process equivalent of the
  event loop's `backend.event_rx.recv()` arm, via `App::handle_ui_event`.
- Observation: `render()` → `Screen` (owned `Buffer` clone). `Screen::text()`
  / `numbered()` / `lines()` / `line(y)` / `find_row(needle)` for text-level
  assertions; `cell(x,y)` / `style(x,y)` / `has_bg(x,y)` / `row_has_bg(y)`
  for visual/style assertions (highlight bars, markers).
- `app()` / `app_mut()` for fixture setup; `actions()` / `drain_actions()`
  for effect assertions.

**Tests added** (8, in `harness::tests`):
- `harness_renders_empty_app_without_panic`
- `type_str_then_enter_emits_send_message`
- `pump_assistant_message_appears_on_screen`
- `screen_text_is_stable_across_renders`
- `find_row_locates_highlighted_block`
- `numbered_includes_row_prefixes`
- `blocks_fixture_renders_user_message`
- `drain_actions_clears_queue`

**Design notes**:
- Module is `#[cfg(test)]`-only. The crate denies `clippy::expect_used` in
  non-test code (`#![deny(...)]` + `cfg_attr(test, allow(...))`), so gating
  test-only avoids that lint without a separate feature. Stage 5 (external
  acceptance driving) will revisit exposing it via a feature.
- `spinner_frame` is stable by construction: `App::new` sets it to 0 and the
  harness never increments it (the real loop does that out-of-band in
  `lib::run`), so no freeze flag is needed for deterministic snapshots.
- `TestBackend` is already provided by `ratatui = "0.29"` — no new dependency.
- The `recursive` test-utils dev-dep addition is the only Cargo.toml change;
  it does not add a new crate, just enables a feature on an existing dep so
  the test suite runs without `--features recursive/test-utils`.

**Quality gates** (in `.worktrees/feat-tui-test-harness`):
- `cargo fmt --all --check` — clean
- `cargo clippy -p recursive-tui --all-targets -- -D warnings` — clean
- `cargo test -p recursive-tui --features recursive/test-utils` — 272
  passed, 0 failed (264 pre-existing + 8 harness)
- Workspace-wide `cargo clippy --all-features` is pre-existing red on
  `recursive-cli` (`web_search`/`http` cfg values not declared in that
  crate's Cargo.toml) — unrelated to this change.

**Next**: stage 2 wires the harness into real visual acceptance tests for
`/resume` highlight alignment, `/theme` panel, and `blocks_from_messages`
reconstruction.
