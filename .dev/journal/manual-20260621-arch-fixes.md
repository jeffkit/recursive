# Manual edit: architecture review fixes

**Date**: 2026-06-21
**Goal**: Fix issues found during deep architecture review — coordinator tool name bug, TokenUsage overflow, dead code cleanup, stale docs, and TUI connection status.

## Files touched

- `src/coordinator.rs` — fix tool name case (PascalCase) in allowlist and denylist; update tests
- `src/tui/cost.rs` — replace silent `as u32` truncation with saturating cast in `estimate_cost()`
- `src/kernel.rs` — update stale module-level doc comment (remove "run() not yet implemented")
- `src/llm/anthropic.rs` — restore `max_search_rounds` field with correct `#[allow(dead_code)]` annotation; remove false annotation on struct
- `src/run_core.rs` — remove unused `permission_mode` field + dead `PermissionMode` import
- `src/multi.rs` — remove unused `config` field from `AgentPool`; keep `_config` in constructor for API compat
- `src/http/mod.rs` — remove false `#[allow(dead_code)]` on `bad_request()` (it IS used in handlers.rs)
- `src/http/rate_limit.rs` — add `#[cfg(test)]` on `bucket_count()` (only used in test code)
- `src/permissions/auto_classifier.rs` — remove false `#[allow(dead_code)]` on `reason` field (used on line 145)
- `src/tui/events.rs` — add `RuntimeReady` variant to `UiEvent`
- `src/tui/backend.rs` — emit `RuntimeReady` after worker initialisation
- `src/tui/app/event_loop.rs` — handle `RuntimeReady` → set `app.connected = true`
- `src/tui/ui/status.rs` — use `app.connected` to show "local" vs "starting…"; update test

## Tests added

- Updated `status_bar_includes_model_and_tokens` to assert both "starting…" (pre-ready) and "local" (post-ready) states

## Quality gates

- `cargo clippy --all-targets --all-features -- -D warnings` → 0 errors, 0 warnings ✓
- `cargo test --workspace` → 1282 passed, 0 failed ✓

## Notes

- BUG-1 (coordinator tool names) was a silent functional bug: the coordinator mode's allowlist/denylist never matched any registered tool, making the whole coordinator filtering a no-op. Now uses correct PascalCase names.
- BUG-2 (TokenUsage truncation) is a theoretical overflow that would only manifest in sessions with >4B tokens — still correct to fix it properly.
- `AnthropicProvider::max_search_rounds` kept (public API) but cannot remove `#[allow(dead_code)]` because `complete_with_search` doesn't exist for Anthropic yet.
