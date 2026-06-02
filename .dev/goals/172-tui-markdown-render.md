# Goal 172 — TUI Markdown rendering for Assistant blocks

**Roadmap**: Phase 14 — TUI Polish (part 1/3)

**Design principle check**:
- Implemented as: pure rendering transformation inside `transcript.rs`; no agent loop changes
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

Assistant reply text is currently rendered as raw plaintext. Bold (`**text**`),
italics (`*text*`), inline code (`` `code` ``), fenced code blocks, and bullet
lists are extremely common in LLM output. Rendering them visually reduces reading
effort and brings Recursive's TUI in line with fake-cc's `Markdown.tsx`.

## Scope (do exactly this, no more)

### 1. Add `pulldown-cmark` dependency

In `Cargo.toml`, under the `[dependencies]` section, add:

```toml
pulldown-cmark = { version = "0.12", default-features = false, optional = true }
```

Add `"dep:pulldown-cmark"` to the `tui` feature list alongside `syntect`.

### 2. New helper `src/tui/ui/markdown.rs`

Create a new file `src/tui/ui/markdown.rs` that exposes one public function:

```rust
/// Parse `text` as Markdown and return ratatui `Line`s for display.
/// Falls back to raw lines if parsing produces nothing.
pub fn render_markdown(text: &str, wrap_width: u16) -> Vec<Line<'static>>
```

Supported elements (minimum viable set):
- **Bold** `**text**` → `Style::default().bold()`
- *Italic* `*text*` → `Style::default().italic()`
- `Inline code` `` `code` `` → `Style::default().fg(Color::Cyan)`
- Fenced code blocks ` ``` ` → each line prefixed with `│ ` in `Color::Cyan`
- Unordered lists `- item` / `* item` → prefixed with `• `
- Ordered lists `1. item` → prefixed with `N. `
- Horizontal rules `---` → a line of `─` chars filling `wrap_width`
- Plain paragraphs → rendered as-is

Keep it simple: iterate `pulldown-cmark` events, accumulate `Span`s into `Line`s.
Do NOT try to implement nested bold+italic or table rendering — those can come later.

### 3. Wire into `src/tui/ui/transcript.rs`

In the `render_assistant_text` function (the one that renders `TranscriptBlock::Assistant`
plain text content), replace the current raw `text.lines()` iteration with a call to
`markdown::render_markdown(text, area_width)`.

The area width should be passed in from the caller's `Rect`. If `area_width` is 0,
fall back to 80.

Export `render_markdown` from `src/tui/ui/mod.rs` (or keep it module-private to `ui`
— the transcript module just needs to call it).

### 4. Tests

In `src/tui/ui/markdown.rs` tests module:
- `bold_renders_as_bold_span`: assert span modifier is bold
- `inline_code_renders_cyan`: assert span fg is Cyan
- `fenced_code_block_prefixed`: assert lines start with `│ `
- `bullet_list_prefixed`: assert `• ` prefix
- `plain_text_passthrough`: plain text → at least one line with the text
- `empty_string_returns_empty`: no panic on empty input

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- Unit tests for all 6 cases pass
- No panic on empty text or text with only whitespace

## Notes for the agent

- `pulldown-cmark` iterates `Event`s: `Start(Tag::...)`, `Text(...)`, `End(Tag::...)`, `Code(...)`, etc.
- Build a `Vec<Span>` for the current line, push to `Vec<Line>` on paragraph breaks or newlines.
- `Line<'static>` requires owned strings in spans — use `.into_owned()` on `CowStr`.
- `syntect` is already a dep but NOT needed for this goal — just use ratatui `Style` modifiers.
- Do NOT modify `src/tui/app.rs`, `src/tui/backend.rs`, `src/tui/commands.rs`, `src/tui/events.rs`, or anything outside `src/tui/ui/`.
- **DO NOT modify files outside**: `src/tui/ui/transcript.rs`, `src/tui/ui/markdown.rs` (new), `src/tui/ui/mod.rs`, `Cargo.toml`.
