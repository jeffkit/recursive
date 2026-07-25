# Manual edit: ctx gauge fallback estimate

**Date**: 2026-07-22
**Goal**: Fix TUI context gauge not updating when LLM provider doesn't return `usage`.
**Files touched**: `src/run_core.rs`
**Tests**: All 37 existing `run_core` tests pass. `cargo clippy --all-targets --all-features -- -D warnings` clean. `cargo fmt --all` clean.
**Notes**:

### Problem
The TUI context gauge (`ctx 14.9K/1.0M`) only updates when the `AgentEvent::Usage` event is emitted. This event was only emitted when `completion.usage` was `Some(...)` (line 205). Some providers (e.g. MiniMax, some OpenAI-compatible APIs) don't reliably return `usage` in every response, so the gauge stays frozen at the value from the first LLM call.

Confirmed by user report: ctx updated from 14.9K to 62.3K on turn 4 (where provider happened to return usage), but stays frozen on turns 2-3 (no usage returned).

### Fix
Added a fallback in `src/run_core.rs::dispatch_llm_step`: when `completion.usage` is `None`, estimate token counts from message content lengths (~4 chars/token) and emit `AgentEvent::Usage` with the estimate. The `total_usage` accumulator is only updated when real provider data is available, so cost tracking stays accurate.

### Key changes
- Added `estimate_prompt_tokens(messages: &[Message]) -> u32` — sums content lengths, divides by 4, min 1
- Modified the `usage` emission block to always emit, with fallback branch using `estimate_prompt_tokens` for input and `content.len() / 4` for output; cache fields set to 0
