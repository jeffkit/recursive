# Goal 54 — Sub-Agent Permission Inheritance

**Roadmap**: follow-up — Sub-Agent permission_hook inheritance

**Design principle check**:
- Implemented as: modification to sub_agent tool delegation logic.
  Parent's `permission_hook` is passed down to child agent.
- Does NOT branch inside `agent.rs::Agent::run`'s main loop (modifies
  how sub-agent is constructed, not the loop itself).

## Why

Goal-43 added `permission_hook` to `AgentBuilder`. Goal-40 added sub-agent
delegation. Currently, when a sub-agent is spawned, it does NOT inherit the
parent's `permission_hook` — the child runs with no permission checks. This
is a security gap: if a parent is configured to deny `run_shell` on certain
commands, a sub-agent can bypass that restriction.

## Scope (do exactly this, no more)

### 1. Find where sub-agent is constructed

The sub-agent / delegate tool creates a new `Agent` via `Agent::builder()`.
Find this code (likely in `src/tools/delegate.rs` or similar) and wire the
parent's `permission_hook` into the child's builder.

### 2. Make `PermissionHook` shareable

Currently `PermissionHook` is `Box<dyn Fn(...) -> ... + Send + Sync>`.
To share between parent and child, it needs to be `Arc`-wrapped:
```rust
pub type PermissionHook = Arc<dyn Fn(&str, &serde_json::Value) -> PermissionDecision + Send + Sync>;
```

Update `AgentBuilder::permission_hook()` to accept `impl Into<Arc<...>>`
or keep the ergonomic `F: Fn + Send + Sync + 'static` signature but
internally wrap in `Arc`.

### 3. Pass hook to sub-agent

When the delegate tool constructs the child agent, if the parent has a
permission_hook, clone the `Arc` and set it on the child builder.

### 4. Tests

- Test: sub-agent inherits parent's permission_hook
- Test: sub-agent denied tool returns error to parent model
- Test: sub-agent without parent hook runs unrestricted (backward compat)
- Test: permission_hook can be cloned (Arc semantics work)

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- Existing permission_hook tests unchanged
- Sub-agent respects parent's permission decisions
- No new dependencies

## Notes for the agent

- Search for "sub_agent" or "delegate" in `src/` to find where child
  agents are built.
- The `PermissionHook` type change from `Box` to `Arc` is the key
  enabler. `Arc<dyn Fn>` is `Clone`, `Box<dyn Fn>` is not.
- Be careful: changing the public type signature of `PermissionHook` is
  a breaking change for library consumers. If possible, keep the builder
  API accepting `F: Fn + 'static` and do the Arc-wrapping internally.
- The Hook system (g48) is separate from PermissionHook. Don't confuse
  them. This goal is specifically about the older `permission_hook`
  callback, not the new `Hook` trait.
