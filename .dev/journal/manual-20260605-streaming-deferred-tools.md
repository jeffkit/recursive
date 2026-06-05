# Manual edit: streaming-deferred-tools

**Date**: 2026-06-05
**Goal**: Fix streaming path to support deferred tool loading (ToolSearchTool)
**Files touched**:
- `src/llm/mod.rs` — added `stream_with_search` trait method with default fallback
- `src/llm/anthropic.rs` — added `stream_with_search` override + `run_stream_search_loop` + `stream_with_body` helpers
- `src/run_core.rs` — streaming path now calls `stream_with_search` instead of merging all tools eagerly

**Tests added**: none (TUI WIP state prevents test compilation; existing LLM tests unaffected)

**Notes**:
- Root cause: `run_core.rs` streaming path was passing all tools (eager + deferred merged) directly to
  `LlmProvider::stream`, so the model saw full schemas for every tool and never needed ToolSearchTool.
  The non-streaming `complete_with_search` path was correct.
- Fix: new `stream_with_search` trait method parallels `complete_with_search`. AnthropicProvider
  implements it via `run_stream_search_loop`, which injects ToolSearchTool into the eager list and
  handles multi-round streaming (each round is a fresh SSE stream; ToolSearch calls are resolved with
  `tool_reference` blocks between rounds, identical to the non-streaming search loop).
- Default trait impl merges all tools and calls `stream` — OpenAI and mock providers get correct
  behavior for free with no code change.
- The streaming search rounds cap at MAX_SEARCH_ROUNDS (3), same as non-streaming.
