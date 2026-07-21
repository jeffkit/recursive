# Manual edit: tui-model-picker-hotswap

**Date**: 2026-07-20
**Goal**: Replace the read-only `/model` panel with an interactive,
scrollable model picker (à la Claude TUI) that hot-swaps the active
LLM provider immediately — no restart required.
**Files touched**:
- `src/kernel.rs` — added `AgentKernel::set_llm` (hot-swap the provider).
- `src/runtime.rs` — added `AgentRuntime::set_llm` delegating to kernel.
- `crates/recursive-tui/src/events.rs` — new `UserAction::SwitchModel`
  and `UiEvent::ModelSwitched` variants.
- `crates/recursive-tui/src/runtime_builder.rs` — new
  `build_provider_for_model(preset_id, model)` that resolves a preset
  from the bundled catalog + `providers.d`, resolves the API key
  (preset `key_env` env var → config `api_key` fallback), and builds a
  `ChatProvider`. Includes unit tests for unknown-preset, missing-key,
  preset-key-env, and config-fallback paths.
- `crates/recursive-tui/src/backend.rs` — `worker_loop` handles
  `SwitchModel`: builds a provider, `rt.set_llm(...)` swaps it in,
  emits `UiEvent::ModelSwitched` (or `UiEvent::Error` when offline /
  build fails). Integration tests for the ready + offline paths.
- `crates/recursive-tui/src/commands.rs` — rewrote `cmd_model` to open
  an interactive `CommandPanelState` picker (header, `▶` cursor, `✓`
  on the active model, "Not configured" banner when no API key).
  Added `ModelPickerEntry`, `collect_model_picker_entries`,
  `model_picker_state`, `build_model_picker_lines`, plus
  `serde_model_picker_context` / `parse_model_picker_context` for the
  panel context round-trip. Removed the old read-only
  `build_model_lines`. Also pinned `RECURSIVE_HOME` in
  `build_cost_lines_computes_per_token_costs` (test-isolation fix —
  see Notes).
- `crates/recursive-tui/src/app/mod.rs` — added `App::active_preset`
  to track the live preset id (config file is not rewritten on
  hot-swap, so re-reading it would show stale data).
- `crates/recursive-tui/src/app/state.rs` — initialise
  `active_preset` from `Config::from_env()` at startup.
- `crates/recursive-tui/src/app/commands.rs` — `model` arms in
  `rebuild_panel_lines_for_selection` and `confirm_command_panel`
  (Enter parses context → `UserAction::SwitchModel`). New panel
  tests for rebuild, confirm, and malformed-context.
- `crates/recursive-tui/src/app/event_loop.rs` — `UiEvent::ModelSwitched`
  handler updates `model_name` + `active_preset` and pushes a System
  note. New test for the handler.

**Tests added**:
- `runtime_builder::build_provider_for_model_*` (4)
- `backend::switch_model_emits_model_switched`,
  `backend::switch_model_offline_emits_error`
- `command_panel_tests::rebuild_model_updates_panel_lines`,
  `command_panel_tests::panel_enter_model_emits_switch_model`,
  `command_panel_tests::panel_enter_model_malformed_context_pushes_error`
- `event_loop::model_switched_updates_model_name_and_preset`

**Notes**:
- Hot-swap is in-memory only: the config file is **not** rewritten, so
  the next session still starts from the on-disk config. `App::active_preset`
  keeps the UI's `✓` honest for the live session.
- `build_provider_for_model` mirrors `Config::from_env()`'s API-key
  resolution (preset `key_env` first, then config `api_key`) so a
  provider built mid-session matches the one built at startup.
- Test-isolation fix: `build_cost_lines_computes_per_token_costs` was
  racing with env-mutating tests — `pricing_for` reads the catalog
  under `RECURSIVE_HOME`, and a parallel test flipping that env var
  caused the two `pricing_for` calls (one in the test, one inside
  `build_cost_lines`) to disagree. Pinned `RECURSIVE_HOME` via
  `PinnedRecursiveHome` (same pattern as `pricing_for_known_models`).
  Verified pre-stash state passed; the flake only surfaced once the
  new env-mutating backend/runtime_builder tests were added.
- Gates: `cargo test --workspace` ✅, `cargo clippy --all-targets
  --all-features -- -D warnings` ✅, `cargo fmt --all --check` ✅,
  `.dev/scripts/tui-test-presence.sh` ✅. TUI mutation gate
  (`tui-mutants.sh`) not run — advisory for manual edits per
  CLAUDE.md; survivors in untouched regions are pre-existing debt.

## Follow-up (same day): filter unconfigured presets + fix cursor

**Problem**: The first cut listed every bundled preset's models regardless
of whether the user had authenticated that provider, and the orange
highlight bar didn't track the `▶` cursor when the "Not configured"
banner was shown (`list_offset` was hardcoded to 2 but the banner added
2 extra rows, so the bar landed on the banner, not the model). User
feedback: "如果没有配置，是不能乱显示的，而且都不能上下选择".

**Changes**:
- `crates/recursive-tui/src/runtime_builder.rs` — new public
  `preset_key_available(preset)`: true when the preset's own `key_env`
  env var is set & non-empty, or the preset is keyless (`key_env == ""`,
  e.g. local `ollama`). Deliberately does **not** honour the global
  `RECURSIVE_API_KEY`/config fallback — that fallback is for custom
  single-key setups and would otherwise make every bundled preset
  "available" (the exact noise the user objected to).
- `crates/recursive-tui/src/commands.rs` —
  `collect_model_picker_entries(active_preset)` now filters presets by
  `preset_key_available` OR `is_active` (the running preset stays
  selectable even without a key, so the live model can be
  re-confirmed). Dropped the `configured` flag and the "Not configured"
  banner from `model_picker_state` / `build_model_picker_lines` —
  unconfigured providers are no longer in the list at all, so the banner
  can't mislead. `cmd_model` returns a helpful `Error` when the filtered
  list is empty (points at per-provider key envs + `recursive init`).
  `list_offset` stays 2 (header + blank) and now always aligns with the
  `▶` row because no banner is ever inserted.
- Tests: replaced the two banner tests with
  `collect_model_picker_entries_filters_unconfigured_presets` (anthropic
  absent without `ANTHROPIC_API_KEY`, present with it — robust to other
  keys being set in the surrounding env) and
  `collect_model_picker_entries_keeps_active_preset_without_key`.
  Updated `collect_model_picker_entries_is_sorted_and_nonempty` and
  `build_model_picker_lines_marks_active_and_selected_rows` for the new
  signatures.

**Gates**: `cargo test --workspace` ✅ (684 TUI tests, stable across 3
runs), `cargo clippy --all-targets --all-features -- -D warnings` ✅,
`cargo fmt --all --check` ✅, `tui-test-presence.sh` ✅.

## Follow-up 2 (same day): show current custom model + Ctrl+P/N + brighter hint

**Problem**: User feedback on the filtered picker:
1. The running `glm-5.2` (a custom provider, no preset) didn't appear in
   the list — user asked if it was pagination. It wasn't; the model just
   isn't offered by any preset.
2. Up/down seemed to have no effect / no highlight bar.
3. Wanted Ctrl+N / Ctrl+P to move down / up (emacs convention).
4. The bottom hint text was `DarkGray` — unreadable on black; wanted it
   to match the boot splash's grey.

**Diagnosis**: A harness render test (`model_panel_marker_row_carries_highlight_bg`)
proved the orange highlight bar *does* land on the `▶` row — the render
code is correct, so (2) was a stale-binary artifact on the user's side.
The real code bug behind (1) was that `build_model_picker_lines` derived
the header's "current: …" from `entries[active_idx]` instead of the real
running model, so when the active model wasn't in any preset the header
lied (it showed `entries[0]`).

**Changes**:
- `crates/recursive-tui/src/commands.rs`:
  - `model_picker_state` now returns the real current model name too, and
    prepends a synthetic `ModelPickerEntry { preset_id: "", …,
    preset_name: "Current (custom provider)" }` when the active model
    isn't found in any preset — so the running model always appears in
    the list with a `✓`.
  - `build_model_picker_lines` takes `current: &str` (the real running
    model) for the header, so the header is always honest. Removed the
    in-panel footer line (it duplicated the panel's `with_hint` and was
    the too-dark text the user objected to).
  - `parse_model_picker_context` now accepts an empty `preset_id` (the
    synthetic sentinel) instead of rejecting it; only an empty `model`
    is malformed.
  - `cmd_model` passes `app.model_name` to the builder and updates the
    hint to advertise Ctrl+P/Ctrl+N.
- `crates/recursive-tui/src/app/commands.rs`:
  - `confirm_command_panel` "model" arm: an empty `preset_id` (synthetic
    current row) is a no-op — pushes a System "Already using {model}."
    note and emits no `SwitchModel` (which would otherwise fail with
    "unknown provider preset").
  - `handle_command_panel_key` refactored: Up/Down share a new
    `panel_move_cursor(delta)` helper, and Ctrl+P / Ctrl+N (CONTROL
    modifier) route to the same helper. They reach the panel because the
    `CommandInteract` dispatch in `handle_key` runs before the prompt's
    own Ctrl+P/N line-edit handlers.
- `crates/recursive-tui/src/ui/command_menu.rs`: `render_command_interact_panel`'s
  `hint_style` changed from `Color::DarkGray` to `Color::Rgb(110, 110, 110)`
  — the same grey the boot splash uses, so the footer is readable on black.

**Tests added**:
- `harness::model_panel_marker_row_carries_highlight_bg` (render-layer
  proof the orange bar lands on the `▶` row),
  `harness::model_panel_shows_current_custom_model_as_synthetic_entry`
  (header names the real model + synthetic row appears with `✓`).
- `command_panel_tests::panel_enter_model_synthetic_current_is_noop`,
  `panel_ctrl_p_moves_cursor_up`, `panel_ctrl_n_moves_cursor_down`,
  `panel_ctrl_n_clamps_at_last_item`.

**Gates**: `cargo test --workspace` ✅ (690 TUI tests, stable across 3
runs), `cargo clippy --all-targets --all-features -- -D warnings` ✅,
`cargo fmt --all --check` ✅, `tui-test-presence.sh` ✅.

## Follow-up 3 (same day): viewport scrolls to follow the cursor

**Problem**: User feedback on follow-up 2: only the footer colour was
visibly better. The list still didn't show `glm-5.2`; paging had no
response; after pressing ↑ many times the highlight finally appeared;
"应该有很多预设的模型，但是屏幕并没有针对内容的显示进行相应滚动".

**Root cause**: `render_command_interact_panel` rendered
`panel.lines.iter().take(content_rows)` — always the *first* `content_rows`
lines, never applying `panel.scroll` and never following the cursor. The
panel height is capped at `MAX_VISIBLE + 2 = 10` rows (~7–8 content rows),
so any list longer than that had its bottom off-screen. When the cursor
(`selected + list_offset`) sat below the visible window (e.g. the active
model far down a long list), the `▶` and the orange bar were off-screen;
pressing ↑ walked the cursor up until it entered the top window. This is
exactly the "press ↑ many times → highlight appears" symptom. It also
explains why `glm-5.2` wasn't visible: its row was below the window, and
the user couldn't scroll to it.

**Fix** (`crates/recursive-tui/src/ui/command_menu.rs`):
`render_command_interact_panel` now computes a viewport that follows the
cursor. Each render it derives the effective scroll read-only from
`panel.scroll` (the sticky "last window") and `cursor_line`:
- if the cursor is above the window, scroll up to it;
- if the cursor is below the window, scroll down so it sits on the last
  content row;
- otherwise keep the sticky scroll (no jitter).
The visible window is then `lines[scroll .. scroll+content_rows]`, and
the highlight is applied to the absolute line `== cursor_line` (i.e.
visible row `cursor_line - scroll`). The `▶` marker is in the line
content at the same absolute line, so the bar and the marker stay
aligned as the cursor moves. No mutation of `panel.scroll` is needed in
the render path (it stays the sticky anchor), so the window only moves
when the cursor leaves it.

**Test added**: `harness::model_panel_viewport_follows_cursor_for_long_list`
— builds a 30-row picker with the cursor at row 25 and asserts the `▶`
is on screen, carries the highlight bar, and that the cursor's model
(`m-25`) is visible (not the top-of-list `m-0`). This fails on the old
`.take(content_rows)` renderer (the `▶` at line 27 would be off-screen),
so it pins the fix.

**Gates**: `cargo test --workspace` ✅ (691 TUI tests, stable across 3
runs), `cargo clippy --all-targets --all-features -- -D warnings` ✅,
`cargo fmt --all --check` ✅, `tui-test-presence.sh` ✅.
