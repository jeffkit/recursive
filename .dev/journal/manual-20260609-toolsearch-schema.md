# Manual edit: toolsearch-schema

**Date**: 2026-06-09
**Goal**: Fix ToolSearchTool returning only a name array to non-Anthropic endpoints that don't support `tool_reference` blocks (DeepSeek, MiniMax Anthropic-compatible endpoints). The agent received `["WebFetch"]` but never got the actual schema, causing it to loop on ToolSearchTool without ever being able to call WebFetch.

**Files touched**:
- `src/llm/mod.rs` — new `native_tool_reference()` trait method (default `false`)
- `src/llm/anthropic.rs` — override `native_tool_reference()` with auto-detect + `with_native()` builder
- `src/tools/tool_search.rs` — `native` field on `ToolSearchTool`; full schemas returned when `native=false`
- `src/tools/mod.rs` — `freeze_deferred_specs(native: bool)` signature change
- `src/runtime.rs` — pass `native_tool_reference()` to `freeze_deferred_specs`
- `tests/deferred_tool_loading.rs` — add `.with_native(true)` to the test provider

**Tests added**:
- `execute_native_returns_json_array_of_names` — verifies name-array mode for official Anthropic API
- `execute_non_native_returns_full_schemas` — verifies full ToolSpec JSON returned for non-native endpoints

**Notes**:
Root cause: `ToolSearchTool::execute` always returned `["WebFetch"]` (a name array). The official Anthropic API converts this to `tool_reference` blocks via `serialize_messages_anthropic`, which the API then expands into full schemas server-side. However, DeepSeek and MiniMax Anthropic-compatible endpoints do not implement the `tool_reference` beta feature, so the model received the name array as plain text, couldn't extract a schema, and kept re-calling ToolSearchTool.

Fix: `AnthropicProvider::native_tool_reference()` returns `true` only when `base_url` contains `api.anthropic.com` (or when explicitly overridden via `.with_native(true)`). All other Anthropic-protocol endpoints get full JSON schemas inline so the model can call the tool immediately.
