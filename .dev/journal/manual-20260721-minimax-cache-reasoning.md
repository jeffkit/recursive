# Manual edit: openai parser reads `prompt_tokens_details.cached_tokens` and `completion_tokens_details.reasoning_tokens`

**Date**: 2026-07-21
**Goal**: Fix two related bugs in `OpenAiProvider` that hid prompt-cache stats and reasoning-token counts for OpenAI-compatible providers (most visibly MiniMax-M3 in the TUI).

**Symptom (user-reported)**: Under TUI on MiniMax-M3 the 📦 cache-hit-rate badge never appears, even after warm-up. Same session under DeepSeek shows it. Switching to MiniMax-M3 also understates cost totals.

**Root cause** (verified by curling the live MiniMax-M3 API):

| Field | OpenAI standard | MiniMax-M3 actual |
|---|---|---|
| prompt cache hits | `prompt_cache_hit_tokens` (top-level) | `usage.prompt_tokens_details.cached_tokens` (nested) |
| reasoning tokens | `completion_tokens_details.reasoning_tokens` | `usage.completion_tokens_details.reasoning_tokens` (same shape — already correct, just never extracted in the non-streaming path) |

`OpenAiProvider::ResponseUsage` only knew the top-level field names, so MiniMax's `cached_tokens` was silently dropped → `cache_hit_tokens == 0` → TUI's `if turn_cache > 0` guard (status.rs:117) suppressed the badge. The streaming path also dropped `completion_tokens_details.reasoning_tokens` (the openai stream comment flagged this as a Goal 273 follow-up).

**Verification before the fix** (4 curl calls against `api.minimaxi.com/v1/chat/completions`, `MiniMax-M3`):

| Call | prompt_tokens | cached_tokens | Δ |
|---|---|---|---|
| #1 fresh, 181 prompt | 181 | 128 | (MiniMax already caches a 128-token prefix server-side) |
| #2 same prompt | 181 | 128 | stable |
| #3 fresh, 780 prompt | 780 | 128 | only old prefix |
| #4 same long prompt | 780 | **768** | real cache hit — proof of caching |

**Files touched**:
- `src/llm/openai.rs` (+268 / -16)
  - `ResponseUsage` struct: added `prompt_tokens_details: Option<PromptTokensDetails>` and `completion_tokens_details: Option<CompletionTokensDetails>`.
  - New helper structs `PromptTokensDetails { cached_tokens: Option<u32> }` and `CompletionTokensDetails { reasoning_tokens: Option<u32> }`, both `#[serde(default)]` so existing payloads keep parsing.
  - `ResponseUsage::to_token_usage` rewritten to:
    - Read cache hit from top-level field first (DeepSeek / OpenAI native), fall back to `prompt_tokens_details.cached_tokens` (MiniMax).
    - Prefer explicit `prompt_cache_miss_tokens`; if absent and `cache_hit > 0`, derive `miss = prompt_tokens.saturating_sub(cache_hit)` so the TUI cache-hit percentage reflects reality instead of pegging at 100%. This honours the documented `hit + miss == prompt_tokens` invariant in `TokenUsage`.
    - Read reasoning tokens from `completion_tokens_details.reasoning_tokens` (previously hard-coded to 0).
  - `process_sse_line` (streaming path) given the same nested reads + the same miss-derivation rule, and `reasoning_tokens` now extracted from `completion_tokens_details`.
  - Five new tests added to the openai `tests` module.

**Tests added** (all passing; 35 openai tests, 3029 workspace tests):
- `parses_minimax_cached_tokens_nested_under_prompt_tokens_details` — full MiniMax-M3 usage JSON; asserts `cache_hit = 768`, invariant `hit + miss == prompt_tokens`, `reasoning = 19`.
- `derives_cache_miss_when_only_hit_is_reported` — MiniMax-shape payload with only `cached_tokens`; asserts `miss = 780 - 768 = 12`.
- `explicit_cache_miss_takes_precedence_over_derived` — DeepSeek-style payload with both explicit miss and a conflicting nested `cached_tokens: 999`; asserts explicit wins (top-level hit also wins over nested).
- `parses_reasoning_tokens_in_non_streaming_path` — o1/o3-style payload; asserts `reasoning_tokens = 175`.
- `stream_usage_parses_minimax_nested_cache_and_reasoning` — drives `process_sse_line` directly with a MiniMax-style SSE chunk; asserts both fields are extracted and content accumulates normally.

**Quality gates** (per `CLAUDE.md`):
- `cargo test --workspace` → 3029 passed, 0 failed.
- `cargo clippy --all-targets -- -D warnings` → clean.
- `cargo fmt --all` → clean.

**Behaviour change for users**:
- TUI: 📦 badge appears on MiniMax-M3 from turn 1; rate stays <100% on cold prefix, climbs toward 100% as the prompt warms (correct cache semantics, not 0% always).
- Cost: MiniMax-M3 sessions now bill the reasoning tokens that were previously dropped, fixing an undercharge on thinking-mode calls.
- DeepSeek / OpenAI / GLM / Gemini / other providers: no behaviour change — top-level field read order is preserved as primary, so existing call sites see identical numbers.

**Not done** (intentional):
- Anthropic provider not touched — it already uses `cache_read_input_tokens` + `cache_creation_input_tokens` correctly; no MiniMax-style bug there.
- No `ResponseUsage` field for `completion_tokens_details.audio_tokens` or similar — only the two fields that affect the TUI badge / cost total.

---

## Follow-up (same session): streaming request was missing `stream_options.include_usage`

**Symptom** (user-reported after rebuild): "I restarted the TUI, the 📦 badge still doesn't show up."

**Investigation**:
- The parser fix from this morning was correct and tests pass.
- The user rebuilt and restarted the TUI binary (`target/debug/recursive-tui` was timestamped after my source edit).
- Compared MiniMax vs DeepSeek over curl with `stream:true`:

  | Provider | `stream_options` set? | Last chunk has `usage`? | `cached_tokens` present? |
  |---|---|---|---|
  | MiniMax-M3 | no | ❌ every chunk has `usage:null` | ❌ |
  | MiniMax-M3 | yes | ✅ final chunk has full usage | ✅ (`114` on cold prompt) |
  | DeepSeek | no | ✅ always emits usage | ✅ |
  | DeepSeek | yes | ✅ (same; flag is ignored) |

  MiniMax treats the final usage chunk as opt-in via `stream_options.include_usage`. Without that flag the SSE stream ends after `[DONE]` and `process_sse_line` never sees a usage block — `cache_hit_tokens` stays 0, the TUI's `if turn_cache > 0` guard suppresses the badge.

  DeepSeek happens to always emit usage (it's how its users detect cache hit rate) so this was masked there.

**Root cause** (in code): `OpenAiProvider::stream_inner` set `body["stream"] = true` but never set `body["stream_options"]`. Confirmed by `grep -n "stream_options" src/llm/openai.rs` → only the comment in `build_request` mentioning the option existed.

**Fix** (also in `src/llm/openai.rs`):

```rust
body["stream"] = Value::Bool(true);
// Ask providers that gate usage emission behind a flag (MiniMax,
// OpenAI native) to include the final usage chunk. Providers that
// always emit usage (DeepSeek) ignore the field harmlessly. Without
// this the streaming path never sees `usage.prompt_tokens_details`
// and the TUI cache-rate badge stays invisible on MiniMax-M3.
body["stream_options"] = serde_json::json!({ "include_usage": true });
```

**New test**: `stream_request_includes_include_usage_flag` — spawns a TCP server, drives `stream()` with `MiniMax-M3`, captures the request body, asserts `stream == true` and `stream_options.include_usage == true`. This regression-closes the gap between the parser fix and the request builder.

**Lesson (for future reference)**: OpenAI's SSE spec makes the final usage chunk opt-in by design — providers differ on whether they emit it without the flag (DeepSeek does, MiniMax doesn't, OpenAI native does, GLM varies). The safe default for any OpenAI-compatible adapter that wants usage data is to always set `stream_options.include_usage = true`. The flag is also documented to be a no-op when unsupported, so there's no downside.

**Final state after both fixes**:
- `cargo test --workspace` → 3030 passed, 0 failed (was 3029 before this follow-up).
- `cargo clippy --all-targets -- -D warnings` → clean.
- `cargo fmt --all` → clean.
- TUI binary `target/debug/recursive-tui` rebuilt at 10:24:12 with both the parser and request-builder fixes.