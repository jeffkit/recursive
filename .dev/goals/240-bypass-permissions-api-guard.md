# Goal 240 — Block BypassPermissions from per-request API callers

**Roadmap**: Arch-review bugfixes (security)

**Design principle check**:
- Implemented as: server-side guard in `parse_permission_mode` + config flag
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

`PermissionMode::BypassPermissions` can currently be set by any caller via
the `permission_mode` field in the HTTP request body. This lets any
authenticated API client silently disable the entire permission system and
execute arbitrary shell commands. The fix is to make `BypassPermissions`
opt-in at the **server** level, not the request level.

## Scope (do exactly this, no more)

### 1. `src/config.rs` — add `allow_bypass_permissions: bool`

Add a field:

```rust
/// If false (default), HTTP API callers cannot request BypassPermissions mode.
pub allow_bypass_permissions: bool,
```

Load from env var `RECURSIVE_ALLOW_BYPASS_PERMISSIONS=1` (default `false`).
Add `allow_bypass_permissions: false` to the test/default config literal in
`src/config.rs` test helpers (the `Config { ... }` in `#[cfg(test)]`).

### 2. `src/http/handlers.rs` — guard `parse_permission_mode`

Change `parse_permission_mode` signature to accept the config flag:

```rust
fn parse_permission_mode(s: &str, allow_bypass: bool) -> PermissionMode {
    match s.trim().to_lowercase().as_str() {
        "bypass" | "bypass_permissions" => {
            if allow_bypass {
                PermissionMode::BypassPermissions
            } else {
                PermissionMode::Default
            }
        }
        "plan" | "plan_mode" => PermissionMode::PlanMode,
        "accept_edits" => PermissionMode::AcceptEdits,
        _ => PermissionMode::Default,
    }
}
```

Pass `state.config.allow_bypass_permissions` at both call sites (lines ~90
and ~212 in `handlers.rs`).

### 3. Update all test `Config { ... }` literals

Any `Config { ... }` literal in `tests/` or `src/` that is missing
`allow_bypass_permissions` needs the field added with value `false`.
Check: `grep -rn "Config {" tests/ src/` and add the field where missing.

### 4. Tests

In `src/http/handlers.rs` `#[cfg(test)]` (or `tests/http.rs`): add a test
that sends `permission_mode: "bypass"` in the request body when
`allow_bypass_permissions = false` (default) and verifies the effective mode
is NOT `BypassPermissions` (the request proceeds normally, not rejected).

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `BypassPermissions` cannot be set via API when `allow_bypass_permissions = false`
- `allow_bypass_permissions = true` (via env var) still allows it

## Notes for the agent

- Read `src/http/handlers.rs` lines 85-160 for the current `parse_permission_mode`.
- Read `src/config.rs` to see where other bool env-var fields are parsed.
- Do NOT reject the request with an error — silently downgrade to `Default` mode.
- **DO NOT modify** `src/agent.rs`, `src/run_core.rs`, `src/runtime.rs`, `src/llm/`.
- **DO NOT call `exit_plan_mode` or `request_plan_mode`.** You are running
  headless; the plan gate has no reviewer. Just read and edit directly.
