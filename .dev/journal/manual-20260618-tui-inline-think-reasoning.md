# Manual edit: tui-inline-think-reasoning

**Date**: 2026-06-18
**Goal**: Fix the TUI not displaying thinking/reasoning content. The
thinking text was visible while streaming but vanished once the turn
finalised — only the answer text remained.

**Root cause**: The user's DeepSeek deployment emits chain-of-thought
*inline* in the `content` field wrapped in `<think>…</think>` tags,
rather than via the dedicated OpenAI-compatible `reasoning_content`
SSE field. During streaming the partial (often unclosed) tag rendered
as plain text, so it was visible; once the full `<think>…</think>`
block landed, `tui::ui::markdown::render_markdown` parsed it as an HTML
block and silently dropped the entire section (HTML / InlineHtml events
are ignored). The dedicated `AgentEvent::Reasoning` → `TranscriptBlock::Reasoning`
pipeline never fired because `Completion::reasoning_content` stayed `None`.

Confirmed empirically: feeding `<think>…</think>\n\nThe answer is 42.`
to `render_markdown` returned only `The answer is 42.`.

**Fix**: Added `Completion::extract_inline_reasoning()` (+ private
`split_think_tags` helper) in `src/llm/chat.rs`. When `reasoning_content`
is empty and `content` contains a `<think>` block, the inner text is
moved into `reasoning_content` and stripped from `content`. Handles the
unclosed-tag case (truncated response) and is a no-op for true reasoner
models that already populate `reasoning_content`. Called once in
`run_core.rs` right after `call_llm` returns, so the existing reasoning
event pipeline lights up and the answer renders cleanly. Provider-agnostic
(single choke point covers TUI / CLI / HTTP).

**Files touched**:
- `src/llm/chat.rs` — `extract_inline_reasoning` method, `split_think_tags`
  helper, and a new `#[cfg(test)] mod tests` (5 cases).
- `src/run_core.rs` — make `completion` mutable and call
  `extract_inline_reasoning()` after `call_llm`.

**Tests added**:
- `extract_inline_reasoning_moves_think_block_to_reasoning`
- `extract_inline_reasoning_handles_inline_single_line`
- `extract_inline_reasoning_unclosed_tag_treats_rest_as_reasoning`
- `extract_inline_reasoning_no_tag_is_noop`
- `extract_inline_reasoning_preserves_existing_reasoning_field`

**Quality gates**:
- `cargo test --workspace` — clean.
- `cargo clippy --all-targets --all-features -- -D warnings` — clean.
- `cargo fmt --all` — applied.

**Notes**:
- Streaming UX: the raw `<think>` text still streams into the assistant
  block live (PartialToken forwards content verbatim); at turn finalise
  the reasoning is hoisted into the thinking block above and the answer
  body re-renders without the tags. Filtering the live stream itself
  would require teaching the SSE parser about the tag mid-stream;
  deferred as it's a cosmetic-during-stream concern only.
- Kept the tag set to `<think>`/`</think>` (DeepSeek-R1 convention).
  `<thinking>` and other variants are not handled.
