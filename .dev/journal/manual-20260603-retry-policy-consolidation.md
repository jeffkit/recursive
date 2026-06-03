# Manual edit: retry-policy-consolidation

**Date**: 2026-06-03
**Goal**: Eliminate `RetryPolicy` duplication across provider files and remove repeated retry loops inside `OpenAiProvider`.
**Files touched**:
- `src/llm/mod.rs` — added shared `RetryPolicy` struct + `Default` + `backoff_for()` implementation; added `use std::time::Duration`; re-exported `RetryPolicy` as part of the public `llm` module surface
- `src/llm/anthropic.rs` — removed local `RetryPolicy` definition; replaced with `use super::RetryPolicy` via the updated `super::` import
- `src/llm/openai.rs` — removed local `RetryPolicy` definition (41 lines); added `post_json_with_retry()` private method that centralises the HTTP + retry loop for non-streaming requests; refactored `complete()` and `complete_structured()` to delegate to it (each shrunk from ~80 lines to ~15); `stream_inner()` retains its own loop (needs the raw `Response` for SSE parsing) but has a clarifying comment
- `src/lib.rs` — updated `pub use` from `llm::openai::RetryPolicy` → `llm::RetryPolicy`
- `src/cli/builder.rs`, `src/main.rs` — updated three call sites from `recursive::llm::anthropic::RetryPolicy` → `recursive::llm::RetryPolicy`

**Tests added**: none (existing `policy_caps_backoff_at_max` and sibling tests in `openai.rs` cover the shared policy; all pass)
**Notes**:
- `stream_inner()` in both providers cannot use `post_*_with_retry()` because a successful 2xx response must be handed to `parse_sse_stream()` as a live `reqwest::Response` — reading the body as text would consume it. Both stream methods retain their own retry loops with a clarifying comment.
- `AnthropicProvider::complete()` now delegates to the already-existing `post_with_retry()` that `run_search_aware_loop` was already using — this was a simple oversight where one caller was missed when `post_with_retry()` was first introduced.
- `RetryPolicy` is now the single source of truth; any change to back-off semantics (e.g. adding 429 handling) only needs to happen once.
