# Goal 281 — External hook `mode: open | closed` field

**Roadmap**: Phase 17 (Production Hardening) — P1 from
`docs/review/architecture-review-2026-06-15.md` (NEW-HOOK-15),
also referenced in 06-10 NEW-HOOK-6.

**Design principle check**:
- Implemented as: extend the external hook JSON schema to include
  a per-hook `mode` field ("open" default, "closed" opt-in);
  route the default decision on timeout/parse-error through that
  flag instead of hardcoding `HookAction::Continue`.
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- ❌ Does NOT add a new feature flag (the per-hook mode is a hook
  *config*, not a runtime toggle)

## Why

`src/hooks/external.rs:727` has a hardcoded fail-open default:

```rust
/// Fail-open: returns `HookResult::continue_default()` on timeout or error.
```

Every external hook (PreToolUse, PostToolUse, UserPromptSubmit,
SessionStart, etc.) falls through to `HookAction::Continue` when:
- the hook command times out (default 60s)
- the hook command exits non-zero
- the hook stdout is unparseable JSON
- the hook binary is missing

For *notification* hooks (PostToolUse to log to a SIEM,
SessionStart to warm a cache) fail-open is correct. For *gate*
hooks (PreToolUse to validate dangerous bash commands,
PreToolUse to enforce a directory allowlist) fail-open is a
security hole — the agent proceeds with the dangerous action
because the gate's "no" was lost in transit.

Today, hook authors have *no* way to opt into fail-closed. The
only mitigation is wrapping the hook command in `set -e` and
hardcoding "if my hook didn't explicitly allow, refuse" — but
that requires changing the hook's *internal* semantics, not its
interaction with the pipeline.

The fix: add a `mode` field to the external hook JSON schema,
default "open" (preserves current behavior), opt-in "closed"
makes timeouts/errors convert `Continue` to `Skip` /
`PermissionDenied` per the hook's intent.

## Scope (do exactly this, no more)

### 1. Extend hook JSON schema

In `src/hooks/external.rs`, locate the hook discovery / loading
code (grep for `HookDefinition` or whatever struct holds the
parsed TOML/JSON config). Add:

```rust
/// Fail behavior for this hook when the command times out,
/// errors, or returns unparseable output.
///
/// - `Open` (default): treat as `HookAction::Continue` — same
///   as today. Use for notification hooks.
/// - `Closed`: treat as `HookAction::Skip` (for PreToolUse /
///   UserPromptSubmit that gate) or `PermissionDenied` (for
///   bash allowlist hooks). Use for security-sensitive gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum HookFailMode {
    #[default]
    Open,
    Closed,
}
```

Add the field to the hook config struct (wherever
`command`, `event`, `timeout` etc. live):

```rust
#[serde(default)]
pub fail_mode: HookFailMode,
```

### 2. Apply the mode at execution time

In the execution path (`src/hooks/external.rs` around line 431-541),
the timeout / error branches currently return
`HookResult::continue_default()`. Change to:

```rust
let result = match tokio::time::timeout(timeout, child.wait_with_output()).await {
    Err(_elapsed) => {
        tracing::warn!(
            hook = %hook_name,
            event = %event_name,
            "hook timed out after {timeout:?}"
        );
        HookResult::from_fail_mode(hook.fail_mode, "hook timed out")
    }
    Ok(Err(io_err)) => {
        tracing::warn!(
            hook = %hook_name,
            event = %event_name,
            error = %io_err,
            "hook command failed to run"
        );
        HookResult::from_fail_mode(hook.fail_mode, &format!("hook error: {io_err}"))
    }
    Ok(Ok(output)) if !output.status.success() => {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(
            hook = %hook_name,
            event = %event_name,
            exit_code = ?output.status.code(),
            stderr = %stderr,
            "hook exited non-zero"
        );
        HookResult::from_fail_mode(
            hook.fail_mode,
            &format!("hook exited {:?}", output.status.code()),
        )
    }
    Ok(Ok(output)) => {
        // ... existing parse path unchanged ...
    }
};
```

Add the helper method on `HookResult`:

```rust
impl HookResult {
    pub fn from_fail_mode(mode: HookFailMode, reason: &str) -> Self {
        match mode {
            HookFailMode::Open => Self::continue_default(),
            HookFailMode::Closed => Self {
                action: HookAction::Skip, // or Error(reason) depending on hook type
                stdout: String::new(),
                stderr: reason.to_string(),
                duration_ms: 0,
            },
        }
    }
}
```

Decide per hook-event-type whether Closed maps to `Skip` or
`Error(...)`. PreToolUse / UserPromptSubmit gates: `Error(reason)`.
SessionStart / SessionEnd notifications: `Continue` (Closed
doesn't apply). Encode this in `from_fail_mode` or a more
specific helper.

### 3. Tests

In `src/hooks/external.rs` `mod tests`:

```rust
#[tokio::test]
async fn closed_hook_times_out_returns_error_not_continue() {
    // Build an external hook with a command that sleeps 10s,
    // timeout 100ms, fail_mode = Closed. Invoke. Assert the
    // returned HookResult.action is NOT Continue.
}

#[tokio::test]
async fn open_hook_times_out_returns_continue() {
    // Same as above but fail_mode = Open (or unset → default).
    // Assert HookResult.action IS Continue.
}

#[tokio::test]
async fn closed_hook_exits_nonzero_returns_error() {
    // Hook command `false`, fail_mode = Closed. Assert non-Continue.
}
```

## Acceptance

- `cargo test --workspace` — green (existing + 3 new tests)
- `cargo clippy --all-targets --all-features -- -D warnings` —
  clean
- `cargo fmt --all` — applied
- `grep "fail_mode\|HookFailMode" src/hooks/external.rs` —
  ≥ 6 matches: enum def, struct field, 2 from_fail_mode call
  sites, 3 tests
- Existing hooks (without `mode` set) still fail-open — verified
  by running the existing test suite, which must pass unchanged

## Notes for the agent

- The `from_fail_mode` mapping from `Closed` to `Skip` vs `Error`
  is the trickiest design decision. Read each hook event type's
  downstream consumer to see what makes sense:
  - `PreToolUse`: skip = no tool call, error = tool rejected →
    both are valid "no" signals; pick `Error(reason)` to make
    the LLM aware
  - `UserPromptSubmit`: skip = pass through, error = reject →
    `Error(reason)` is more explicit
  - `PostToolUse`: skip/continue indistinguishable — Closed
    should map to Continue (Closed is a no-op for PostToolUse)
  - `SessionStart` / `SessionEnd`: Continue
- Document the mapping in `HookFailMode`'s doc comment.
- Estimated diff: 1 file (external.rs), ~60 lines net.
- **Test discipline reminder (from g268 post-mortem)**: tests
  must use real `tokio::time::timeout` (don't fake by calling
  the inner function directly) so the timing path is exercised.

**Disjoint file guarantee**: This goal touches only
src/hooks/external.rs. Safe to run in parallel with every other
goal in this batch.