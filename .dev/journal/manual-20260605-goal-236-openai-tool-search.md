# Manual edit: goal-236-openai-tool-search

**Date**: 2026-06-05
**Goal**: Implement OpenAI provider software-layer ToolSearch fallback (goal-236)
**Files touched**:
- `src/llm/openai.rs` — 440 insertions

**Tests added**:
- `openai_search_loop_skips_when_no_deferred`
- `openai_search_loop_resolves_deferred_tool`
- `openai_search_loop_caps_at_max_rounds`

**Notes**:
- Self-improve loop failed 5 times before manual intervention:
  - 2× HTTP 404 (minimax, deepseek) — root cause: `~/.recursive/config.toml`
    has `type = "anthropic"`, which overrode missing `RECURSIVE_PROVIDER_TYPE`
    in `apply_provider_profile`. Fixed in self-improve.sh (commit c0d8b01).
  - 3× `stuck:Write:3` — deepseek-chat tried to Write the full openai.rs
    (~73K chars) in one call, exceeding max_tokens and producing truncated
    JSON arguments. The `parse_completion` fallback silently returned `{}`
    for malformed arguments, causing "missing path" errors. Goal file was
    updated with a CRITICAL note to use apply_patch, but deepseek-chat
    ignored it regardless — HITL triggered.
- Implementation mirrors `AnthropicProvider::run_search_aware_loop` but in
  software: plain JSON tool_result instead of `tool_reference` content blocks.
- `complete_with_search` fast-path: if `deferred_tools` is empty, skip the
  loop and call `complete()` directly (no extra round-trip, no ToolSearchTool
  in the request).
- Both `run_search_loop` and `run_stream_search_loop` use `Box::pin` for
  recursive async calls.
