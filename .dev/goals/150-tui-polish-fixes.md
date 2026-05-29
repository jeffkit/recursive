# Goal 150 — TUI polish: model name from config, scroll cap, brightened palette, basic markdown

**Roadmap**: TUI revamp series follow-up (Goal 143-149 reported by user)

**Design principle check**:
- Pure UI / display fixes in `crates/recursive-tui/`
- Adds one new file: `crates/recursive-tui/src/ui/markdown.rs`
- Does NOT touch core lib

## Why

User feedback after running the post-Goal-149 TUI on macOS Terminal:

1. **Status bar shows `gpt-4o-mini`** even when `~/.recursive/config.toml`
   sets `model = "deepseek-v4-flash"`. Goal 149 fixed `Backend::build_runtime`
   to honour the config file but **left `app::detect_model_name` reading
   only env vars** — so the runtime correctly talks to DeepSeek but the
   status bar lies about it.
2. **Transcript can't scroll up**. PageUp/↑ have no effect on long
   conversations; the user can never re-read prior messages.
3. **Palette is too dim** on macOS Terminal's default dark profile —
   `Color::DarkGray` on side gutters and `Color::Cyan` on assistant
   text both look greyed out, hard to read.
4. **Markdown is rendered as raw text** — `## Heading`, `**bold**`,
   `` `code` `` all show their literal markers. Goal 144 explicitly
   deferred markdown rendering, but it bites enough that we should do
   the minimum (4-construct inline parser).

## Scope (do exactly this, no more)

### 1. Status bar reads config.toml

`crates/recursive-tui/src/app.rs::detect_model_name`:

- Same priority chain as `Backend::build_runtime`:
  `RECURSIVE_MODEL` → `OPENAI_MODEL` → `~/.recursive/config.toml`'s
  `[provider].model` → `"gpt-4o-mini"` default
- Use `recursive::config_file::FileConfig::load()` (already public)
- ~10-line change

### 2. Scroll cap uses real wrapped line count

`crates/recursive-tui/src/ui/chat.rs`:

- Replace `let total_lines = lines.len() as u16;` with
  `messages_widget.line_count(inner_width)`
- ratatui 0.29 has `Paragraph::line_count(width: u16) -> usize`
  for exactly this purpose
- Compute `inner_width = area.width - 2` (left + right border)
- ~5-line change

### 3. Brighten the palette

In `crates/recursive-tui/src/ui/transcript.rs` and
`crates/recursive-tui/src/ui/diff.rs` and
`crates/recursive-tui/src/ui/input.rs`:

- Bulk replace `Color::DarkGray` → `Color::Gray` for gutters,
  separators, latency labels, footer hints, system / compacted
  blocks
- User block label: `Color::White` → `Color::LightBlue` (brighter,
  matches the gutter)
- Assistant block label: `Color::Cyan` → `Color::LightCyan` (sharper)
- Assistant body text: `Color::Cyan` (one big block of cyan in the
  screenshot was the most painful) → driven by markdown renderer,
  defaulting plain text to `Color::White`
- Status bar (`ui/status.rs`) and modal (`ui/modal.rs`): unchanged —
  they already use White / DarkGray on a coloured background and
  read fine in the screenshot

### 4. Basic markdown renderer

New file `crates/recursive-tui/src/ui/markdown.rs`:

- `MdState { in_code_block: bool }` — fenced code blocks toggle this
  across consecutive lines
- `RenderedLine { spans: Vec<Span<'static>>, state: MdState }`
- `render_inline(line: &str, default_fg: Color, state: MdState) -> RenderedLine`
- Constructs handled: **bold**, *italic* / _italic_, `inline code`,
  `# heading`, `## heading` ..., `- bullet` / `* bullet` / `+ bullet`,
  ``` ``` ``` fenced code blocks
- Anything else → plain `Span::styled(text, Style::default().fg(default_fg))`
- Bold = `LightCyan + BOLD`, italic = `default_fg + ITALIC`,
  code = `LightYellow`, heading = `LightCyan + BOLD` (no hashes),
  bullet = `LightYellow • ` + body
- ~250 lines incl. unit tests
- **Not** a CommonMark conformance pass — links, tables, blockquotes,
  reference-style links, escaping all out of scope

`render_assistant` in `transcript.rs`:

- Iterate over `text.lines()`, feed each through
  `markdown::render_inline`, thread `MdState` so fenced blocks span
  multiple lines

### 5. Tests

- `markdown::*` — 9 unit tests covering each construct + plain text +
  empty line + unmatched marker (already in the new file)
- Existing `transcript::*` tests should keep passing — most assert
  text content, not styles. The few that assert specific colours need
  small updates.
- New: `app::detect_model_name_reads_config_file_when_env_unset`
  using `recursive::test_util::PinnedHome` (same pattern Goal 149
  introduced for `backend::offline_mode_and_config_file_resolution`)

### 6. Not in scope

- ❌ Tables, links, blockquotes, footnotes, reference-style links
- ❌ Syntax highlighting inside fenced code blocks
- ❌ Nested italic/bold combinations
- ❌ Auto-linking URLs
- ❌ Smart-quotes / em-dash conversion
- ❌ Right-side gutter / line numbers
- ❌ Theme system (single hardcoded palette)

## Acceptance

1. `cargo build -p recursive-tui` passes
2. `cargo test --workspace` all green
3. `cargo clippy --all-targets --all-features --workspace -- -D warnings` clean
4. `cargo fmt --all -- --check` clean
5. Manual: with the user's existing `~/.recursive/config.toml`
   pointing at DeepSeek, status bar shows `deepseek-v4-flash`
6. Manual: long conversation → PageUp scrolls all the way to the
   first message; scroll_offset clamping no longer cuts it short
7. Manual: assistant message with `**bold**`, `## heading`, `` `code` ``,
   `- bullets` renders with formatting (no literal `**`/`##`/`` ` ``
   visible)
8. Manual: gutters and side bars are visible (no longer "almost
   black on black")

## Notes

- Markdown parser is intentionally byte-level and tolerant — LLMs emit
  malformed markdown all the time and we don't want to crash on it.
- `parse_inline` does **one pass** with a small state machine; no
  regex.
- Code block fence (` ``` `) is detected by `trimmed.starts_with("```")`,
  so leading whitespace before the fence is OK.
- The render path stays `Vec<Line<'static>>` so the existing
  `Paragraph::scroll` mechanism still works.
