# Manual edit: quality-fixes-r3

**Date**: 2026-06-22
**Goal**: Address code quality issues identified in audit round 3 (from /tmp/recursive-analysis-3.md)
**Files touched**:
- `src/http/handlers.rs`
- `src/session/mod.rs`
- `src/tui/runtime_builder.rs`

**Tests added**: none (modified existing test structure)

**Notes**:

## Changes made

### MEDIUM-2: AGUI driver task JoinHandle monitoring (`src/http/handlers.rs`)

The `tokio::spawn` at line 1505 that drives the agent run was previously
discarding its JoinHandle entirely. If the task panicked, the panic was
silently swallowed (tokio drops panics when the JoinHandle is dropped).

Fix: assigned the handle to `driver_handle`, then spawned a lightweight
watcher task that logs the panic via `tracing::error!` if it occurs.

Note: full cancellation on SSE disconnect was not implemented in this pass
because the current code has no `CancellationToken` infrastructure wired
through `AgentRuntime`. The panic monitor is the safe, minimal fix that
makes failures observable without requiring a broader refactor.

### LOW-2: Remove redundant `entries.clone()` (`src/session/mod.rs`)

In `ExportedTranscript::from_session_dir`, `entries` was cloned to move
into `messages`, then used once more for `.len()`. Reordered to compute
`message_count` first and then move `entries` directly, eliminating the
unnecessary heap allocation.

### MEDIUM-1: Env var mutation in test has RAII guard (`src/tui/runtime_builder.rs`)

The `offline_mode_and_config_file_resolution` test manually cleared
`RECURSIVE_API_KEY` / `OPENAI_API_KEY` and restored them at the end, but
offered no protection if an assertion panicked mid-test (env vars would
remain cleared for subsequent tests).

Fix: introduced a local `ApiKeyGuard` RAII struct in the test module. The
guard clears the two env vars on construction and restores them on drop
(including panic paths). Note that `PinnedRecursiveHome` already acquires
the global `env_lock()`, so concurrent env-mutation tests are serialised;
the new guard adds panic-safety only.

## Skipped issues

- **HIGH-1** (once_hook test race): requires changes to hook state-machine
  semantics and/or shell script timeout — deferred.
- **MEDIUM-3** (config deny_unknown_fields): adding this attribute carries
  backward-compatibility risk for users with existing config files that may
  have extra fields. Skipped pending explicit decision on compatibility policy.
- **LOW-1** (once hook doc): documentation-only, deferred.

## Validation

All quality gates passed:
```
cargo fmt --all        ✓
cargo clippy -- -D warnings  ✓
cargo test --workspace  ✓ (0 failures)
```
