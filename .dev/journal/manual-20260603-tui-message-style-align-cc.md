# Manual edit: tui-message-style-align-cc

**Date**: 2026-06-03
**Goal**: Align TUI transcript block styles with Claude Code — User
block becomes a `> text` quoted-style highlight; Assistant block
becomes a bullet-only body (no "Agent" header, no inline latency /
streaming text); ToolCall and ToolResult are merged into a single
function-call unit whose bullet colour reflects the tool's
running / success / error state.

**Files touched**:
- `src/tui/model.rs` — added `ToolResultData` struct; `ToolCall`
  variant gained `result: Option<ToolResultData>`; removed
  standalone `ToolResult` variant.
- `src/tui/app/mod.rs` — re-exports `ToolResultData` alongside
  the other `TranscriptBlock` types.
- `src/tui/app/event_loop.rs` — imports `ToolResultData`;
  `UiEvent::ToolResult` handler now looks up the matching
  `ToolCall` block (most recent first) and fills in its `result`
  field; falls back to synthesising a `ToolCall` if no match
  exists. `write_file` path additionally stamps the original
  `ToolCall` as completed so the bullet turns green (the
  synthesised `Diff` block still carries the change).
  `toggle_last_expandable` walks back over completed `ToolCall`
  blocks. Three existing tests in this file were updated to
  match the new `ToolCall { result: Some(..) }` shape.
- `src/tui/ui/transcript.rs` — `render_user` is now a `> text`
  block on a dim grey background highlight (multi-line input
  stays stacked with one `>` per line). `render_assistant` leads
  with a cyan `•` bullet and the body sits under a 2-space indent;
  uses `markdown::render_markdown` (already wired on main) and
  threads the bullet only on the first line. The standalone
  `render_tool_result` was removed; `render_tool_call` is the
  single renderer and picks a colour based on the result: Yellow
  `⏺` + `Running…` body when the tool is in flight, Green `⏺` on
  success, Red `⏺` on failure. Size hint and 3-line collapse
  with Ctrl+E hint are preserved. The block-level `render_block`
  dispatch was simplified to drop the deleted `ToolResult` arm.
- `src/tui/keymap.rs` — Ctrl+E test updated to push a `ToolCall`
  block with `Some(result)` instead of a separate `ToolResult`
  block.
- `src/tui/ui/transcript.rs` (tests) — the old "label and body",
  "latency", and "streaming" assertions were replaced with
  bullet/colour assertions; new tests for paragraph-break bullets,
  background highlight on user blocks, and the running / success /
  failure bullet colours.
- `src/tui/app/commands.rs` (tests) — the `Ctrl+E` tool-result
  tests now match `ToolCall { result: Some(...), .. }` instead
  of the deleted `ToolResult` variant.

`cargo test --workspace` all green.
`cargo clippy --workspace --all-targets --all-features -- -D warnings`
clean.

**Notes**:
- The `▎ Agent` header and inline `⏱` / `…streaming` markers are
  removed from the assistant block on purpose — the user's
  reference (Claude Code) doesn't use them, and the status bar
  still surfaces the spinner + `⏱ Xs` while a turn is running, so
  the in-block markers were redundant. If we later decide to put
  the latency back, it should live in the status bar (where it
  already does) rather than on the block.
- This commit is the second attempt of the same work — the
  first attempt's branch (`feat/tui-bugfix`) was wiped when an
  external agent cleared the worktree, then again when
  `git rebase main` (after a refactor split `app.rs` into
  `app/{mod,state,event_loop,commands,render}.rs`) produced
  conflicts. The user opted to reset and reapply against the new
  module layout; the code is byte-equivalent to the first pass.
- Thinking / reasoning rendering is still out of scope here (see
  the companion `tui-reasoning-event-pipeline` commit for that).
