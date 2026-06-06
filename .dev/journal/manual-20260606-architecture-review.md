# Manual edit: architecture-review

**Date**: 2026-06-06
**Goal**: Senior architect review of the Recursive codebase — identify bugs, architecture issues, and security concerns
**Files touched**: (read-only review, no changes made)
**Tests added**: none
**Notes**: Comprehensive review covering all major subsystems

---

# Architecture & Code Review: Recursive v0.6.0

## Executive Summary

Recursive is a well-structured self-improving Rust agent with a clean kernel/runtime split and solid layered permission design. The major risks are concentrated in the HTTP server (session lifecycle), a handful of silent data-loss paths in session persistence, and an unbounded in-memory session store that will OOM under load. The multi-memory system needs consolidation. Security posture is acceptable for local use but needs hardening before any production deployment.

---

## Critical Bugs

### 1. `AgentRuntime::close()` Never Called in HTTP Handlers

**File**: `src/http/handlers.rs`

`AgentRuntime::close()` fires the `SessionEnd` hook, persists the final session state, and tears down background jobs. None of the HTTP route handlers (`run_agent`, `delete_session`, `patch_session`) ever call it. Effects:

- `SessionEnd` hooks never fire for HTTP-hosted sessions.
- Any hook-based cleanup (e.g., writing a summary, flushing telemetry) is silently skipped.
- The `BackgroundJobManager` inside the runtime is never shut down.
- The `session_closed` flag in `AgentRuntime` is never set, so any future guard logic using it will be wrong.

**Recommendation**: Extract a helper that calls `runtime.close().await` and wrap every HTTP handler's terminal path (normal return, error return, and the session-deletion path) with it. A `Drop` impl is not sufficient here because `close()` is `async`.

---

### 2. Audit Data Silently Dropped on Mutex Poison

**File**: `src/session.rs`, `SessionPersistenceSink::emit()`

In the `MessageAppendedWithAudit` arm, when the JSONL writer mutex is poisoned, the recovery path calls:

```rust
sink.write_message(msg, None)   // audit passed as None
```

The `audit` value that was passed into `emit()` is discarded instead of forwarded. This means that on any panic that poisons the lock, all subsequent messages lose their audit trail silently — the file is written but without tool call metadata.

**Recommendation**: Pass `Some(audit)` on the recovery path, or if the lock is considered unrecoverable, return an error instead of silently degrading.

---

### 3. Double Permission Hook Check

**File**: `src/tools/mod.rs`, `ToolRegistry::invoke()` and `invoke_with_audit()`

`invoke()` calls the `permission_hook` first, then calls `invoke_with_audit()`, which for interactive tools re-enters the hook under the `Goal-212` block. Depending on how the hook is implemented, this can cause:

- Two prompts shown to the user for a single tool call.
- Two audit records written.
- State mutations in the hook running twice.

**Recommendation**: Centralise the permission check to a single call site. Either `invoke()` does it and passes a "pre-approved" token to `invoke_with_audit()`, or remove the outer check and let `invoke_with_audit()` own it exclusively.

---

## High-Severity Architecture Issues

### 4. Unbounded In-Memory HTTP Session Store

**File**: `src/http/handlers.rs`, `AppState`

Sessions are stored in `Arc<RwLock<HashMap<String, SessionState>>>` with no eviction, TTL, or size cap. Each session holds a full `AgentRuntime` which includes a transcript (`Vec<Message>`), checkpoint state, and potentially large tool outputs. Under load or with long-lived sessions this will exhaust heap memory and crash the process.

**Recommendation**: Add a session TTL (configurable, e.g., 30 minutes idle) with a background reaper task, or add a max-session count with LRU eviction. Persist sessions to disk on eviction rather than dropping them.

---

### 5. Full Transcript Clone on Every Kernel Turn

**File**: `src/runtime.rs`, `execute_kernel_turn()` (~line 476)

```rust
let messages = self.transcript.clone();
```

The entire transcript is cloned on every turn before being handed to the kernel. For long-running sessions with many turns and large tool outputs this is a significant allocation. At 10,000 chars/turn × 100 turns = ~1 MB clone every turn.

**Recommendation**: Pass a slice or `Arc<Vec<Message>>` / `Cow<[Message]>` to the kernel. Since the kernel only reads the transcript, a shared reference is sufficient and avoids the copy.

---

### 6. `drain_queue()` Swallows Per-Message Errors

**File**: `src/runtime.rs`, `drain_queue()`

When processing queued messages, errors from individual `execute_kernel_turn()` calls are logged but not propagated. Callers cannot distinguish which enqueued messages completed successfully and which failed. In a multi-agent or event-loop scenario, a failed message may cause stale state that corrupts subsequent turns.

**Recommendation**: Return `Vec<Result<TurnOutcome, Error>>` (one per message) or stop on first error with a clear indication of how many messages were processed.

---

### 7. `truncate_label()` Uses Byte Length for Multibyte Strings

**File**: `src/runtime.rs`, `truncate_label()`

```rust
if trimmed.len() < s.len() {  // .len() is byte count, not char count
    format!("{}…", trimmed)
} else {
    trimmed.to_string()
}
```

A CJK string where the last character was cut mid-UTF-8 will show the ellipsis even if no truncation happened (or vice versa). Worse, slicing a `&str` at a byte offset that falls inside a multibyte codepoint will panic at runtime.

**Recommendation**: Use `s.char_indices()` to find the truncation point and compare with `s.chars().count()`.

---

### 8. Brittle `GoalEvaluator` Text-Prefix Parsing

**File**: `src/run_core.rs` (GoalEvaluator logic)

Goal completion detection parses the LLM response by checking whether the first line starts with `"YES"` or `"NO"`. Any preamble, formatting variation, or localisation in the model's response will silently misclassify. A false `NO` keeps the agent looping; a false `YES` terminates it early.

**Recommendation**: Use `complete_structured()` with a JSON schema `{ "completed": bool, "reason": string }` to get a typed response, the same way `Compactor` uses structured output.

---

### 9. `run_event_loop()` Uses `try_lock()` and Can Miss Job Completions

**File**: `src/runtime.rs`, `run_event_loop()`

The background job manager is polled with `try_lock()`. If another coroutine holds the lock when the event loop ticks, the completed-job check is skipped entirely for that tick. In practice this means a completed background job may be ignored for one tick, but in edge cases (high contention) it can be missed longer.

**Recommendation**: Use a `tokio::sync::Notify` or a dedicated completion channel to signal the event loop when a background job finishes, eliminating the polling pattern.

---

### 10. Hand-Rolled UTC Date Math in `session.rs`

**File**: `src/session.rs`, `chrono_lite_now()` and `epoch_day_to_ymd()`

The codebase already depends on `chrono`. Using custom leap-year calculation and epoch-day-to-YMD conversion is unnecessary complexity that is easy to get wrong (e.g., the 400/100/4 rule for leap years) and hard to maintain.

**Recommendation**: Replace with `chrono::Utc::now().format(...)`.

---

## Medium-Severity Design Issues

### 11. Overlapping and Redundant Memory Systems

**File**: `src/tools/` (Remember/Recall, Facts, Scratchpad, EpisodicRecall, WorkingMemory)

Five separate memory tools coexist with no documented guidance on when to use each:

| Tool | Mechanism | Scope |
|------|-----------|-------|
| Remember/Recall | JSONL file | Session-persistent |
| Facts | In-memory map | Turn-scoped |
| Scratchpad | Text file | Session-persistent |
| EpisodicRecall | Vector store | Cross-session semantic |
| WorkingMemory | In-memory Vec | Turn-scoped |

The agent must reason about which system to use without any architectural guidance. This causes both over-use (writing to all of them) and under-use (ignoring useful data from previous turns).

**Recommendation**: Document a clear hierarchy (or consolidate to 2–3 tools). A suggested split: ephemeral scratchpad for intra-turn reasoning, session file for durable cross-turn state, vector store for cross-session recall.

---

### 12. `ToolRegistry::clone()` Shares Mutable State

**File**: `src/tools/mod.rs`

`ToolRegistry` derives or implements `Clone`, but individual tools may hold `Arc<Mutex<...>>` state. Cloning the registry creates a second registry that shares the same underlying state. Callers who clone a registry to get isolation (e.g., for a sub-agent) will get unexpected state sharing.

**Recommendation**: Either document explicitly that clones share state, or implement a `fork()` method that creates a truly independent registry.

---

### 13. MAX_SEARCH_ROUNDS Hard-Coded in LLM Providers

**File**: `src/llm/anthropic.rs`, `src/llm/openai.rs`

`MAX_SEARCH_ROUNDS = 3` is duplicated in both providers with no shared constant or configuration. If a task genuinely needs more rounds (e.g., exploring a very large tool set), there is no way to increase it without editing source.

**Recommendation**: Move to `Config` as a configurable field with a sensible default.

---

### 14. Stuck Detection Uses Fixed Error-Rate Window

**File**: `src/run_core.rs`

`STUCK_WINDOW=10` / `STUCK_ERROR_RATE=0.8` means the agent needs 8 errors in 10 consecutive steps before halting. For short tasks with high-frequency tool calls this may trigger too early; for very long tasks with occasional errors it may never trigger. The window is not adaptive.

**Recommendation**: Make the window and threshold configurable in `Config`. Consider also detecting semantic stuck-ness (repeated identical tool calls) in addition to error-rate-based detection.

---

## Security Issues

### 15. HTTP Auth Disabled by Default

**File**: `src/http/auth.rs`, `src/config.rs`

```rust
// Auth disabled by default (zero-config)
```

The HTTP server starts with no authentication when no `RECURSIVE_API_KEY` or JWT config is provided. Any process on the same host (or network, if the bind address is `0.0.0.0`) can call the API and execute arbitrary shell commands via the `run_shell` tool.

**Recommendation**: For production deployments, require explicit opt-in to disable auth rather than opt-in to enable it. At minimum, log a prominent warning at startup when auth is disabled.

---

### 16. `BypassPermissions` Mode Settable via API Without Authorization Check

**File**: `src/http/handlers.rs`, request parsing

The `PermissionMode::BypassPermissions` variant can be set in the HTTP request body. No additional check verifies that the caller is authorised to request this mode. An authenticated caller can effectively disable the entire permission system.

**Recommendation**: Restrict `BypassPermissions` to a separate admin API key or a server-side configuration flag. Never allow it to be set per-request by regular API callers.

---

### 17. Sensitive Config Values in Environment Variables Without Masking

**File**: `src/config.rs`

API keys and secrets are loaded from environment variables. The `Debug` derive on `Config` will print these values in any debug log, crash report, or `{:?}` format call.

**Recommendation**: Implement a custom `Debug` for `Config` (and any nested struct holding secrets) that replaces secret values with `"[REDACTED]"`.

---

## Low-Priority Observations

### 18. `providers.toml` Context Window Lookup Falls Back to 0

**File**: `src/llm/mod.rs`, `context_window_tokens_for_model()`

If a model is not found in `providers.toml`, the function returns `0`. Downstream code that divides by this value or uses it to bound a loop will silently misbehave.

**Recommendation**: Return `Option<u64>` and handle the unknown-model case explicitly, or return a documented safe default (e.g., 8192 tokens).

---

### 19. `TokenUsage` Arithmetic Can Silently Overflow

**File**: `src/llm/mod.rs`

`TokenUsage` accumulates across turns using plain `u64` addition with `+=`. While overflow is unlikely in practice at 64-bit, the `Add` impl has no overflow check. The cost calculation (`input_tokens as f64 * price`) loses precision for very large token counts.

**Recommendation**: Use `saturating_add` for the token counters.

---

### 20. Sub-Agent Depth Limit via Environment Variable Only

**File**: `src/tools/sub_agent.rs`

`RECURSIVE_SUBAGENT_MAX_DEPTH` is read from the environment at call time, not at startup. A child process can unset or override this variable to remove the depth limit entirely.

**Recommendation**: Read the depth limit once at startup (in `Config`) and pass it through as an immutable value. Child processes should inherit the limit, not be able to override it.

---

## Summary Table

| # | Severity | Category | Description |
|---|----------|----------|-------------|
| 1 | Critical | Bug | `close()` never called in HTTP handlers |
| 2 | Critical | Bug | Audit data dropped on mutex poison |
| 3 | Critical | Bug | Double permission hook check |
| 4 | High | Architecture | Unbounded in-memory session store |
| 5 | High | Performance | Full transcript clone every turn |
| 6 | High | Architecture | `drain_queue()` swallows errors |
| 7 | High | Bug | `truncate_label()` byte vs char count |
| 8 | High | Reliability | Brittle GoalEvaluator text parsing |
| 9 | Medium | Architecture | `try_lock()` polling in event loop |
| 10 | Medium | Maintenance | Hand-rolled date math despite chrono dep |
| 11 | Medium | Architecture | 5 overlapping memory tools |
| 12 | Medium | Architecture | Registry clone shares mutable state |
| 13 | Medium | Maintainability | MAX_SEARCH_ROUNDS duplicated, not configurable |
| 14 | Medium | Reliability | Fixed stuck-detection window |
| 15 | High | Security | HTTP auth disabled by default |
| 16 | High | Security | BypassPermissions settable per-request |
| 17 | Medium | Security | Secrets exposed in Debug output |
| 18 | Low | Reliability | Context window lookup returns 0 on miss |
| 19 | Low | Correctness | TokenUsage plain addition, no overflow guard |
| 20 | Low | Security | Sub-agent depth limit can be env-overridden |
