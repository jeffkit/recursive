# Manual edit: tui-reasoning-event-pipeline

**Date**: 2026-06-03
**Goal**: Wire reasoning / thinking content (DeepSeek R1, OpenAI o1,
…) end-to-end. The provider already accumulates
`reasoning_content` into the returned `Completion`, but the runtime
only stored it on `Message.reasoning_content` and never emitted a
separate event. Add `AgentEvent::Reasoning` plus a
`TranscriptBlock::Reasoning` variant, emit once per step that
produced reasoning, and render as a `thinking…` header + dim grey
italic body.

**Files touched**:
- `src/event.rs` — `AgentEvent::Reasoning { text, step }` variant
  added between `PartialToken` and `Compacted`.
- `src/run_core.rs` — emit `AgentEvent::Reasoning` right after the
  AssistantText emission (and before the tool-call / no-tool-call
  branching) whenever `completion.reasoning_content` is non-empty.
  Single emit point covers both code paths.
- `src/tui/events.rs` — `UiEvent::Reasoning { content }` added to
  carry the full reasoning text to the UI.
- `src/tui/backend.rs` — `map_agent_event` translates
  `AgentEvent::Reasoning` into `UiEvent::Reasoning`.
- `src/tui/app/event_loop.rs` — `handle_ui_event` matches
  `UiEvent::Reasoning` and pushes a `TranscriptBlock::Reasoning`
  block.
- `src/tui/model.rs` — `TranscriptBlock::Reasoning { text }`
  variant.
- `src/tui/ui/transcript.rs` — `render_reasoning` produces a
  yellow-bold `thinking…` header followed by the reasoning text
  in dark grey italics. Empty text still shows the header so the
  user knows the model produced an empty reasoning trace.
  `render_block` dispatch extended to cover the new variant.
  New unit tests: `reasoning_block_has_thinking_header_and_italic_body`,
  `reasoning_block_empty_text_still_shows_header`.

**Quality gates**:
- `cargo test --workspace` — clean.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — clean.

**Notes**:
- This is the **complete-text** variant: providers that stream
  reasoning deltas (DeepSeek's `reasoning_content` SSE deltas) have
  already joined them into a single string by the time the
  `Completion` lands in `run_core`. To get true streaming partial
  display (token-by-token as they arrive), `LlmProvider::stream`
  would need to expose a `reasoning_tx: Option<StreamSender>`
  parallel to `stream_tx`; deferred.
- The `StepEvent` ↔ `AgentEvent` bridge that the previous
  implementation of this commit relied on is gone — commit
  `ef2ac76` (Goal 219) removed the `StepEvent` enum entirely
  and made `RunCore` emit `AgentEvent` directly. So this commit
  adds the new variant straight to `AgentEvent` and skips the
  legacy `StepEvent` arm.
- Anthropic provider's `Completion::reasoning_content` is always
  `None` today; the new event is therefore a no-op for Anthropic
  callers. Once Anthropic surfaces thinking (e.g. extended
  thinking beta), the same plumbing will light up automatically
  because the emit point is provider-agnostic.
- The reasoning block is rendered **before** the corresponding
  Assistant block in the transcript (events arrive in that order
  at the UI), matching Claude Code's layout where `thinking…`
  sits directly above the visible response.
