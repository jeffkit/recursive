# Manual edit: toolsearch-v2

**Date**: 2026-06-09
**Goal**: Fix ToolSearchTool looping on DeepSeek/MiniMax Anthropic-compatible endpoints.
The agent kept calling ToolSearchTool and getting ["WebFetch"] back but never being
able to use WebFetch.

**Files touched**:
- `src/llm/anthropic.rs` — `supports_deferred_tools()` now only returns true for api.anthropic.com
- `tests/deferred_tool_loading.rs` — tests opt-in via RECURSIVE_DEFERRED_TOOLS=true

**Tests added**: none (existing tests updated)

**Notes**:
Root cause (confirmed by reading fake-cc source): `tool_reference` is an Anthropic beta
feature that third-party Anthropic-compatible endpoints (DeepSeek, MiniMax) don't support.
When ToolSearchTool returns `["WebFetch"]` and serialize_messages_anthropic converts it
to tool_reference blocks, the proxy endpoint either ignores or rejects the beta format,
so the model never receives the WebFetch schema.

Claude Code handles this identically via `isFirstPartyAnthropicBaseUrl()` in toolSearch.ts:
deferred tools / ToolSearch is disabled by default for non-api.anthropic.com endpoints.
Users who know their proxy supports tool_reference can set ENABLE_TOOL_SEARCH=true.

Recursive now mirrors this: supports_deferred_tools() returns false unless:
  1. base_url contains "api.anthropic.com", OR
  2. RECURSIVE_DEFERRED_TOOLS=true is set explicitly

For DeepSeek/MiniMax users, WebFetch and other deferred tools are now sent eagerly
(full schema in the initial request) so the model can use them without any ToolSearch
round-trip.
