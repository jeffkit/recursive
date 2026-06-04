# Manual edit: tool-result-pairing-fix

**Date**: 2026-06-04
**Goal**: Fix HTTP 400 errors when Anthropic-mode providers receive multiple tool_use blocks in one assistant turn
**Files touched**:
- `src/llm/anthropic.rs` — added `serialize_messages_anthropic()` + wired to both call sites

**Tests added**: none (existing tests cover serialization paths; integration tested via self-improve runs)

**Notes**:
- Root cause: run_core.rs pushes one `Message` per tool result; Anthropic API requires ALL
  tool_result blocks for a single assistant turn to arrive in ONE user message as an array
  of content blocks. Any model that issues 2+ tool_use blocks per step triggered HTTP 400.
- Fix: `serialize_messages_anthropic()` coalesces consecutive tool-role messages into a single
  `{"role":"user","content":[{...tool_result...},{...tool_result...}]}` object. The in-memory
  Message representation is unchanged — only the JSON serialized for the HTTP request changes.
- Affects `build_request` (simple path) and `build_request_with_partition` (tool-search path).
- Prerequisite fix for running self-improve with anthropic-deepseek and anthropic-minimax providers,
  both of which use multi-tool steps.
