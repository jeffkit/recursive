# Manual edit: Anthropic prompt-cache breakpoints

**Date**: 2026-10-05
**Goal**: Issue #90 (P0) — the Anthropic adapter never sent `cache_control`, so
Anthropic's prompt cache was always a 0-hit: every ReAct step re-billed the full
system prompt + tool schemas + growing history at the regular input price
(5-8x wasted input spend on Sonnet-class models). The metering/pricing side was
already complete (`TokenUsage::cache_hit_tokens`/`cache_miss_tokens`, the
`cache_read_input_tokens`/`cache_creation_input_tokens` parsers, the pricing
discount) — only the request side was missing.

**Change**:
- `AnthropicProvider::supports_prompt_cache()` — resolves the new gate:
  explicit `with_prompt_cache(bool)` > `RECURSIVE_PROMPT_CACHE=1|true|0|false` >
  endpoint heuristic (`base_url` contains `api.anthropic.com`). Official endpoint
  defaults **on**; third-party Anthropic-compatible proxies (DeepSeek, MiniMax,
  …) default **off**, mirroring the existing `supports_deferred_tools()` policy.
- `build_request` gained a `prompt_cache: bool` parameter; when set, the new
  `apply_prompt_cache` emits at most two `cache_control: {"type":"ephemeral"}`
  breakpoints:
  - the last **system** block (system is widened from a bare string into
    `[{type:"text",…}]`) — a system breakpoint caches tools + system in one
    entry, so no redundant tool breakpoint is added; when there is no system,
    the breakpoint moves to the last **tool** instead;
  - the **second-to-last** wire message, leaving the final (still-mutating)
    message outside the cached prefix so the entry stays reusable next step.
  `mark_last_content_block` widens string message content into a text block
  before attaching the marker.

**Files touched**:
- `src/llm/anthropic.rs` — new field/methods, `build_request` signature,
  `apply_prompt_cache` + `mark_last_content_block`, tests
- `docs/architecture/providers/anthropic.md` — documented the behaviour + env var

**Tests added** (`src/llm/anthropic.rs`):
- `prompt_cache_marks_system_and_second_to_last_message`
- `prompt_cache_without_system_anchors_on_last_tool`
- `prompt_cache_widens_plain_text_message`
- `prompt_cache_marks_tool_reference_result_message_at_outer_block`
- `prompt_cache_disabled_leaves_request_untouched`
- `prompt_cache_default_follows_endpoint_and_env_override`
- `stream_request_carries_cache_control_breakpoints` — captures the real
  streaming request body off a mock server, proving the gate is wired into the
  call site and not just `build_request`

**Notes**:
- No new dependencies.
- Existing tests pinned the 6-arg `build_request`; they now pass `false` so the
  legacy (no-cache) wire shape is asserted unchanged.
- The companion concerns from the issue (microcompactor off by default +
  compaction 80% window; unstable skill-reminder tail keeping breakpoint
  positions from being stable) are deliberately **not** touched here.
