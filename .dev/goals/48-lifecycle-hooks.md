# Goal 48 — Lifecycle Hooks

**Roadmap**: 4.4 — Hooks / Lifecycle Events

**Design principle check**:
- Implemented as: new module `src/hooks.rs` that the agent loop *calls into*
  at well-defined lifecycle points; configurable via `AgentBuilder`.
- Does NOT branch inside `agent.rs::Agent::run`'s main loop logic
  (only adds hook invocation points at existing boundaries).

## Why

The agent currently has one callback point: `permission_hook` (pre-tool
execution). Claude Code, Codex, and Hermes all support richer lifecycle
hooks — pre/post tool call, session start/end, pre/post compaction. These
allow consumers to: auto-format after file writes, auto-lint, log tool
duration, inject context, or gate operations. Hooks complete the observer
pattern that StepEvent started.

## Scope (do exactly this, no more)

### 1. `src/hooks.rs` — new module

Define a `HookEvent` enum with variants:
- `PreToolCall { name: &str, args: &Value }` — before tool dispatch
- `PostToolCall { name: &str, args: &Value, result: &str, duration_ms: u64 }` — after tool returns
- `SessionStart { goal: &str }` — at the top of `Agent::run`
- `SessionEnd { outcome: &AgentOutcome }` — just before returning from `Agent::run`
- `PreCompact { transcript_len: usize }` — before compaction fires
- `PostCompact { removed: usize, summary_chars: usize }` — after compaction

Define a trait:
```rust
pub trait Hook: Send + Sync {
    fn on_event(&self, event: HookEvent) -> HookAction;
}
```

Where `HookAction` is:
```rust
pub enum HookAction {
    Continue,         // proceed normally
    Skip,            // skip this tool call (PreToolCall only)
    Error(String),   // abort with error message (PreToolCall only)
}
```

Provide a `HookRegistry` that holds `Vec<Arc<dyn Hook>>` and dispatches
events to all registered hooks in order.

### 2. `src/agent.rs` — inject hook calls

At the five lifecycle boundaries:
1. Start of `run()` → `SessionStart`
2. Before tool dispatch (right after `permission_hook` check) → `PreToolCall`
3. After tool returns → `PostToolCall`
4. Before `maybe_compact` → `PreCompact`
5. End of `run()` (right before returning `Ok(outcome)`) → `SessionEnd`

The existing `permission_hook` remains unchanged for backward compat.
`PreToolCall` hooks that return `Skip` or `Error` have the same effect as
`PermissionDecision::Deny` but are evaluated AFTER the permission hook.

### 3. `AgentBuilder` — wiring

Add `.hook(Arc<dyn Hook>)` method that appends to the builder's hook list.
Multiple hooks are supported; they fire in registration order.

### 4. `src/lib.rs` — re-exports

Export `Hook`, `HookEvent`, `HookAction`, `HookRegistry`.

### 5. Tests

- Unit test: `SessionStart` fires with correct goal text
- Unit test: `PreToolCall` → `Skip` prevents tool execution, feeds error back to model
- Unit test: `PostToolCall` receives correct duration and result
- Unit test: Multiple hooks fire in order
- Unit test: `SessionEnd` receives correct outcome
- Integration: hook that counts tool calls matches step count

## Acceptance

- `cargo test` green, including all new tests
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `HookEvent` variants are `#[non_exhaustive]` for future extension
- Existing `permission_hook` tests still pass unchanged
- No new dependencies

## Notes for the agent

- Read `src/agent.rs` carefully. The tool dispatch is around line 320-410.
  The compact call is `maybe_compact` around line 470-490.
- Don't touch `PermissionHook` — it stays as-is. The new `Hook` trait is
  a separate, more general abstraction.
- For `PostToolCall` duration: measure wall-clock around the
  `self.tools.execute()` call using `std::time::Instant`.
- Keep `HookEvent` variants as borrowed references where possible to avoid
  cloning large values. Use lifetimes if needed, or accept owned `String`
  copies for the fields that need them (tool args are `Value` which is cheap
  to reference).
- `HookAction::Skip` and `HookAction::Error` only make sense for
  `PreToolCall`; for other events they should be treated as `Continue`.
