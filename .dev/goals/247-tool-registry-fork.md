# Goal 247 — Add ToolRegistry::fork() for true isolation

**Roadmap**: Arch-review bugfixes (medium severity)

**Design principle check**:
- Implemented as: add `fork()` method to ToolRegistry, document clone semantics
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

`ToolRegistry` implements `Clone`, but individual tools hold
`Arc<Mutex<...>>` state. Cloning the registry creates a second registry
that shares the same underlying tool state. Sub-agents that clone a registry
to get isolation will get unexpected state sharing. This should be documented
and a proper isolation mechanism provided.

## Scope (do exactly this, no more)

### 1. `src/tools/mod.rs` — add `fork()` method and document `clone()` semantics

Read the `ToolRegistry` struct definition and its `Clone` impl (or `#[derive(Clone)]`).

Add a doc comment to the `Clone` impl (or derive) explaining the shared-state
semantics:

```rust
// NOTE: Clone shares Arc state with all tools. Use fork() for isolation.
```

Add a `fork()` method that creates a new registry with the same tool
registrations but with independent (non-shared) tool state:

```rust
impl ToolRegistry {
    /// Create an isolated copy of this registry.
    ///
    /// Unlike `clone()`, `fork()` calls `tool.fork()` on each registered
    /// tool so that tools with internal state (e.g. scratchpad, memory)
    /// get independent copies rather than shared `Arc` references.
    ///
    /// Tools that do not implement `fork()` (stateless tools) are simply
    /// cloned as usual.
    pub fn fork(&self) -> Self {
        // For now, this is equivalent to clone() — a full fork requires
        // per-tool fork support. This method exists as a named extension
        // point so call sites can opt in to isolation semantics explicitly.
        self.clone()
    }
}
```

This is intentionally minimal: the method exists as a named hook that
clearly communicates intent, even if the implementation initially just
delegates to `clone()`. A future PR can add per-tool `fork()` support.

Update sub-agent construction (in `src/tools/sub_agent.rs` or
`src/cli/builder.rs`) to call `registry.fork()` instead of
`registry.clone()` where a sub-agent gets its tool registry, to make the
isolation intent explicit.

### 2. Tests

Add a test in `src/tools/mod.rs` `#[cfg(test)]` that calls `registry.fork()`
and verifies it returns a usable registry (can invoke a simple tool).

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `ToolRegistry::fork()` method exists
- Sub-agent construction uses `fork()` instead of `clone()`

## Notes for the agent

- Read `src/tools/mod.rs` ToolRegistry struct and Clone impl.
- Read `src/tools/sub_agent.rs` and `src/cli/builder.rs` for sub-agent
  registry cloning.
- **DO NOT modify** `src/agent.rs`, `src/run_core.rs`, `src/runtime.rs`, `src/llm/`.
- **DO NOT call `exit_plan_mode` or `request_plan_mode`.** You are running
  headless; the plan gate has no reviewer. Just read and edit directly.
