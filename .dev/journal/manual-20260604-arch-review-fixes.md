# Manual edit: arch-review-fixes

**Date**: 2026-06-04
**Goal**: Fix high-priority bugs identified in architecture review (arch-review.md)
**Files touched**:
- `src/run_core.rs` — removed `Err(ProviderTruncated)` early return (Issue #25); added orphan-tool-call placeholder on parallel task panic (Issue #60); removed unused `Error` import
- `src/compact.rs` — unified compaction prefix: `render_structured` now outputs `[compacted: structured]` to match `kernel.rs` detection (Issue #13); replaced `expect()` on COMPACT_SCHEMA parse with error logging + fallback (Issue #14)
- `src/mcp.rs` — added `env` field to `McpServer`, propagate `config.env` in `load_mcp_config()`, apply `cmd.envs(env)` in `spawn_stdio` (Issue #30)
- `src/llm/mod.rs` — `RetryPolicy::backoff_for` now treats HTTP 429 as transient (Issue #39)
- `src/tools/shell.rs` — replaced `expect("stdout/stderr piped")` with `ok_or_else(|| Error::Tool {...})?` (Issue #27)
- `src/runtime.rs` — dispatch `SessionStart`, `UserPromptSubmit`, `SessionEnd` hooks in `run()`; `SessionEnd` is skipped when `FinishReason::Cancelled` (Issue #42)
- `src/hooks/mod.rs` — updated stale doc comment on `SessionEnd` variant
- `src/session.rs` — `hash_tool_specs` now logs a tracing error on serialization failure instead of silently returning empty string (Issue #35)
- `src/mcp_server.rs` — added `env: None` to two test `McpServer` struct literals
- `tests/mcp_e2e.rs` — added `env: None` to test `McpServer` literal
- `tests/mcp_integration.rs` — added `env: None` to test `McpServer` literal
- `tests/integration.rs` — updated two test assertions to reflect new hook dispatch behavior
- `src/llm/openai.rs` — updated `policy_does_not_retry_4xx` test to reflect that 429 is now retried

**Tests added**: none (existing tests updated to reflect new behavior)

**Notes**:
- Issue #36 (TUI `Arc::try_unwrap().expect()`) was analyzed but not fixed: after awaiting the spawned task, the Arc clone held by the task is always dropped (Rust unwinds destructors on panic), so `try_unwrap` is infallible in practice. Structural refactor deferred.
- The AGENTS.md exception for `openai.rs` `client build` `expect()` is retained as documented.
- `SessionStart` fires at the beginning of each `run()` turn (not just the first); hooks that need "first turn only" behavior should track state themselves.
