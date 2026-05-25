# Goal 43 — Permission Hooks (Phase 3.4)

> **Roadmap**: feature 3.4, S size, High impact. **Closes Phase 3.**
> **Design principle check**: orthogonal — adds an `AgentBuilder`
> callback that runs *before* each tool execution. Default = no
> callback = current behavior. Pluggable — opt-in via builder.
> Testable — MockProvider + a counting closure.

## What

Add an `AgentBuilder::permission_hook(...)` setter that takes a
closure `Fn(&str, &Value) -> PermissionDecision`:

```rust
pub enum PermissionDecision {
    Allow,
    Deny(String),              // string is the reason fed back to the model as a tool error
    Transform(Value),          // replace the arguments before execution
}
```

The hook is invoked just before `Tool::execute(args)`. If
`Deny(reason)`, the tool error path is taken with the reason as the
message. If `Transform(new_args)`, the new args are passed to
execute. If `Allow`, original args go through unchanged.

## Why

- Production users need to gate destructive operations (e.g. block
  `run_shell` outside CI; require human-in-the-loop for `apply_patch`
  in a protected branch; rate-limit `web_fetch`).
- Sub-agents (g40) ought to inherit a stricter permission policy than
  the parent — this is the API that makes that possible without
  adding new infra.
- The agent loop stays clean — permission concerns live in the
  hook, not in `Tool` implementations.

## API

```rust
// src/agent.rs
pub enum PermissionDecision {
    Allow,
    Deny(String),
    Transform(serde_json::Value),
}

pub type PermissionHook = Box<dyn Fn(&str, &serde_json::Value) -> PermissionDecision + Send + Sync>;

impl AgentBuilder {
    pub fn permission_hook<F>(mut self, hook: F) -> Self
    where
        F: Fn(&str, &serde_json::Value) -> PermissionDecision + Send + Sync + 'static,
    {
        self.permission_hook = Some(Box::new(hook));
        self
    }
}
```

## Tests

- `permission_hook_allow_passes_args_unchanged` — MockProvider asks
  the agent to call `read_file({"path":"x"})`; hook returns Allow;
  assert tool received exact args.
- `permission_hook_deny_returns_error_to_model` — hook returns
  `Deny("not allowed")`; assert the model's NEXT message sees a
  tool error result containing "not allowed". Agent does NOT
  actually execute the tool.
- `permission_hook_transform_replaces_args` — hook returns
  `Transform({"path":"y"})`; assert tool received `path=y`, not `x`.
- `default_no_hook_is_unchanged` — without `permission_hook(...)`,
  existing tests still pass.

## Wiring

- `src/agent.rs`: add types + builder + invoke before
  `tool.execute(...)` in the step loop. Should be ~30 lines.
- No `src/main.rs` change for v1 — CLI doesn't expose hooks. They're
  a library API for embedders.
- No `src/tools/*` change.

## Acceptance

- `cargo build` green.
- `cargo test` green; +4 new tests.
- `cargo clippy --all-targets -- -D warnings` clean.
- `cargo fmt --all -- --check` clean.
- Pre-existing tests pass unchanged (no API break).

## Out of scope (defer)

- Async hooks (return a Future). Sync is enough for v1.
- A CLI flag to load a permission policy from a file.
- Wiring hook into `sub_agent` so child inherits a stricter policy.
  (Natural follow-up after this lands.)
