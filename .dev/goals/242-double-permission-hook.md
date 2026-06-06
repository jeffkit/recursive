# Goal 242 — Fix double permission hook call in ToolRegistry

**Roadmap**: Arch-review bugfixes (critical bug)

**Design principle check**:
- Implemented as: remove outer hook call from `invoke()`, delegate to `invoke_with_audit()`
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

`ToolRegistry::invoke()` calls `permission_hook` first, then calls
`invoke_with_audit()` which calls the hook again for interactive tools
(the block at ~line 808 in `src/tools/mod.rs`). This causes:
- Two permission prompts shown to the user for a single tool call.
- Two audit records written.
- Any stateful hook (e.g. counting approvals, rate limiting) runs twice.

## Scope (do exactly this, no more)

### 1. `src/tools/mod.rs` — remove the outer hook call from `invoke()`

Read `invoke()` (around line 694) and `invoke_with_audit()` (around line 718).

The `invoke()` function currently does something like:

```rust
pub async fn invoke(&self, name: &str, arguments: Value) -> Result<String> {
    let effective_args = if let Some(hook) = &self.permission_hook {
        // calls hook here  <-- REMOVE THIS BLOCK
        ...
    } else {
        arguments
    };
    self.invoke_with_audit(name, effective_args).await.result
}
```

Remove the hook call from `invoke()`. It should become:

```rust
pub async fn invoke(&self, name: &str, arguments: Value) -> Result<String> {
    self.invoke_with_audit(name, arguments).await.result
}
```

The single hook call inside `invoke_with_audit()` (around line 808) is
the canonical one — leave it untouched.

### 2. Tests

Read the existing tests around `test_permission_deny_blocks_invoke` and
`permission_hook_deny_blocks_invoke` (~line 1212, 1264) to confirm they
still pass after the change (they should, since `invoke_with_audit` still
calls the hook).

Add a new test that verifies the hook is called **exactly once** per
`invoke()` call:

```rust
#[tokio::test]
async fn permission_hook_called_exactly_once() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct CountingHook(Arc<AtomicUsize>);
    #[async_trait::async_trait]
    impl PermissionHook for CountingHook {
        async fn check(&self, _name: &str, _args: &Value) -> PermissionResult {
            self.0.fetch_add(1, Ordering::SeqCst);
            PermissionResult::Allow(None)
        }
    }

    let count = Arc::new(AtomicUsize::new(0));
    let reg = ToolRegistry::local()
        .with_permission_hook(Arc::new(CountingHook(count.clone())));
    // register a simple echo tool or use an existing one from local()
    reg.invoke("echo", serde_json::json!({"msg": "hi"})).await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
}
```

Adjust the tool name / arguments to match a tool that actually exists in
`ToolRegistry::local()` (read `build_standard_tools` in `src/tools/mod.rs`
to find a suitable simple tool).

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- Permission hook called exactly once per `invoke()` call
- Existing permission deny/allow tests still pass

## Notes for the agent

- Read `src/tools/mod.rs` `invoke()` and `invoke_with_audit()` in full
  before editing.
- Check that `invoke_with_audit()` truly does call the hook for all tool
  types (not just interactive ones) before removing the outer call.
- **DO NOT modify** `src/agent.rs`, `src/run_core.rs`, `src/runtime.rs`, `src/llm/`.
- **DO NOT call `exit_plan_mode` or `request_plan_mode`.** You are running
  headless; the plan gate has no reviewer. Just read and edit directly.
