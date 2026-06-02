# Goal 174 — TUI Theme picker: switchable colour palettes

**Roadmap**: Phase 14 — TUI Polish (part 3/3)

**Design principle check**:
- Implemented as: new `Theme` struct + palette table; rendering modules read from `AppState`
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

All colours in the TUI are hard-coded literals (`Color::Cyan`, `Color::DarkGray`, etc.)
scattered across 4 UI files. Extracting them into a named `Theme` struct lets users
switch palettes (e.g. a "light" background theme vs the default "dark" theme) without
editing source. This is low-cost, high-coherence work that also de-duplicates the
colour constants.

## Scope (do exactly this, no more)

### 1. New file `src/tui/ui/theme.rs`

```rust
use ratatui::style::Color;

#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub name: &'static str,
    // Status bar
    pub status_bg: Color,
    pub status_fg: Color,
    pub status_mode_fg: Color,   // active mode label
    pub status_cost_fg: Color,   // cost / token counter
    pub status_dim_fg: Color,    // separators / inactive segments
    // Input box
    pub input_border: Color,
    pub input_prompt_fg: Color,
    pub input_bash_fg: Color,
    pub input_note_fg: Color,
    pub input_command_fg: Color,
    pub input_atfile_fg: Color,
    // Transcript
    pub user_bar: Color,         // ▎ colour for user block left bar
    pub assistant_bar: Color,    // ▎ colour for assistant block left bar
    pub system_bar: Color,       // ▎ colour for system / note block left bar
    pub tool_call_icon: Color,   // 🔧 row color
    pub tool_ok_fg: Color,       // ✓ icon
    pub tool_err_fg: Color,      // ✗ icon
    pub code_fg: Color,          // inline code / fenced block prefix
    pub diff_add: Color,         // + lines in diffs
    pub diff_del: Color,         // - lines in diffs
}

pub const DARK: Theme = Theme {
    name: "dark",
    status_bg: Color::DarkGray,
    status_fg: Color::White,
    status_mode_fg: Color::Green,
    status_cost_fg: Color::Cyan,
    status_dim_fg: Color::DarkGray,
    input_border: Color::DarkGray,
    input_prompt_fg: Color::Cyan,
    input_bash_fg: Color::Yellow,
    input_note_fg: Color::Green,
    input_command_fg: Color::Magenta,
    input_atfile_fg: Color::Cyan,
    user_bar: Color::Blue,
    assistant_bar: Color::Green,
    system_bar: Color::DarkGray,
    tool_call_icon: Color::Yellow,
    tool_ok_fg: Color::Green,
    tool_err_fg: Color::Red,
    code_fg: Color::Cyan,
    diff_add: Color::Green,
    diff_del: Color::Red,
};

pub const LIGHT: Theme = Theme {
    name: "light",
    status_bg: Color::Gray,
    status_fg: Color::Black,
    status_mode_fg: Color::Blue,
    status_cost_fg: Color::DarkGray,
    status_dim_fg: Color::Gray,
    input_border: Color::Gray,
    input_prompt_fg: Color::Blue,
    input_bash_fg: Color::Rgb(180, 100, 0),
    input_note_fg: Color::Rgb(0, 120, 0),
    input_command_fg: Color::Rgb(100, 0, 150),
    input_atfile_fg: Color::Blue,
    user_bar: Color::Blue,
    assistant_bar: Color::Rgb(0, 130, 0),
    system_bar: Color::Gray,
    tool_call_icon: Color::Rgb(180, 100, 0),
    tool_ok_fg: Color::Rgb(0, 130, 0),
    tool_err_fg: Color::Red,
    code_fg: Color::Blue,
    diff_add: Color::Rgb(0, 130, 0),
    diff_del: Color::Red,
};

pub const SOLARIZED: Theme = Theme {
    name: "solarized",
    status_bg: Color::Rgb(7, 54, 66),     // base02
    status_fg: Color::Rgb(131, 148, 150), // base0
    status_mode_fg: Color::Rgb(38, 139, 210),  // blue
    status_cost_fg: Color::Rgb(42, 161, 152),  // cyan
    status_dim_fg: Color::Rgb(88, 110, 117),   // base01
    input_border: Color::Rgb(88, 110, 117),
    input_prompt_fg: Color::Rgb(42, 161, 152),
    input_bash_fg: Color::Rgb(181, 137, 0),    // yellow
    input_note_fg: Color::Rgb(133, 153, 0),    // green
    input_command_fg: Color::Rgb(108, 113, 196), // violet
    input_atfile_fg: Color::Rgb(42, 161, 152),
    user_bar: Color::Rgb(38, 139, 210),
    assistant_bar: Color::Rgb(133, 153, 0),
    system_bar: Color::Rgb(88, 110, 117),
    tool_call_icon: Color::Rgb(181, 137, 0),
    tool_ok_fg: Color::Rgb(133, 153, 0),
    tool_err_fg: Color::Rgb(220, 50, 47),  // red
    code_fg: Color::Rgb(42, 161, 152),
    diff_add: Color::Rgb(133, 153, 0),
    diff_del: Color::Rgb(220, 50, 47),
};

pub const ALL_THEMES: &[&Theme] = &[&DARK, &LIGHT, &SOLARIZED];

pub fn find_theme(name: &str) -> &'static Theme {
    ALL_THEMES.iter().copied().find(|t| t.name == name).unwrap_or(&DARK)
}
```

### 2. Export from `src/tui/ui/mod.rs`

Add:
```rust
pub mod theme;
pub use theme::{Theme, DARK, find_theme};
```

### 3. Thread theme through `AppState` in `src/tui/app.rs`

Add `pub theme: &'static crate::tui::ui::theme::Theme` to `AppState`.
Initialise to `&crate::tui::ui::theme::DARK` in `App::new()`.

### 4. Wire colours in rendering modules

**In `src/tui/ui/status.rs`**: replace hard-coded `Color::` literals with
`app.theme.status_*` fields. Pass `&app.theme` (or a reference to the relevant
fields) into the render function. The function signature already takes `&AppState`
so `app.theme` is accessible.

**In `src/tui/ui/input.rs`**: similarly replace `Color::Cyan` / `Color::Yellow`
etc. with `app.theme.input_*` and `app.theme.input_border`.

**In `src/tui/ui/transcript.rs`**: replace `Color::Green` / `Color::Red` in diff
rendering, `Color::Yellow` for tool calls, `Color::Green` / `Color::Red` for
✓/✗, with `theme.diff_add` / `theme.diff_del` / `theme.tool_call_icon` etc.
The transcript render function currently takes `&AppState` — use `app.theme`.

**In `src/tui/ui/chat.rs`** (the top-level layout driver that calls the above):
no change needed if rendering functions receive `&AppState` already.

Do NOT touch `src/tui/ui/modal.rs` or `src/tui/ui/command_menu.rs` for this goal.

### 5. `/theme` command in `src/tui/commands.rs`

Add to `default_set()`:

```rust
CommandSpec {
    name: "theme",
    aliases: &["t"],  // only if "t" not taken
    summary: "Switch colour theme (dark / light / solarized)",
    usage: "/theme <name>",
    handler: CommandHandler::WithArgs(cmd_theme),
},
```

`cmd_theme(app, args)`:
- If `args` is empty → push a `System` block listing available themes and current.
- If `args` matches a known theme name → `app.theme = crate::tui::ui::theme::find_theme(args.trim())`.
- Otherwise → push an error System block "Unknown theme. Available: dark, light, solarized".

This is a sync command operating entirely on `AppState` — no `UserAction` needed.

### 6. Tests

In `src/tui/ui/theme.rs` tests:
- `dark_theme_has_expected_name`: `DARK.name == "dark"`
- `find_theme_returns_dark_for_unknown`: `find_theme("nope").name == "dark"`
- `all_themes_have_unique_names`: no duplicates in `ALL_THEMES`

In `src/tui/commands.rs` tests (add to existing test module):
- `theme_command_is_registered`: find `"theme"` in `default_set()`

Update the command-count test from 14 → 15 (accounting for `/mcp` from Goal-173
being done in a parallel worktree — but this goal's worktree won't have that change,
so just count the commands that exist in THIS worktree's `default_set()` at test time.
**Do not hard-code 15 if /mcp isn't present.** Instead use:
`assert!(count >= 13, "expected at least 13 commands, got {count}")`.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `DARK` and `LIGHT` themes exported and reachable
- `/theme dark` and `/theme light` switch `app.theme` without panicking

## Notes for the agent

- `&'static Theme` in `AppState` avoids heap allocation and Clone.
- The render functions for `status.rs` and `input.rs` currently take `&AppState`
  or receive individual fields — read them carefully before modifying signatures.
- Keep the existing visual appearance identical when the default `DARK` theme is active
  (i.e. DARK theme values must exactly match the current hard-coded colours).
- **DO NOT modify**: `src/tui/ui/modal.rs`, `src/tui/ui/markdown.rs`,
  `src/tui/events.rs`, `src/tui/backend.rs`.
- **Files to touch**: `src/tui/ui/theme.rs` (new), `src/tui/ui/mod.rs`,
  `src/tui/app.rs`, `src/tui/ui/status.rs`, `src/tui/ui/input.rs`,
  `src/tui/ui/transcript.rs`, `src/tui/commands.rs`.
