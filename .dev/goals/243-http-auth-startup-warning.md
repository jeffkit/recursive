# Goal 243 — Log prominent warning when HTTP auth is disabled at startup

**Roadmap**: Arch-review bugfixes (security)

**Design principle check**:
- Implemented as: startup log warning in `src/main.rs` HTTP server init path
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

The HTTP server starts with no authentication when no API key or JWT config
is provided. Any process on the same host (or network, if bound to 0.0.0.0)
can call the API and execute arbitrary shell commands. There is currently no
warning at startup to alert the operator of this risk.

## Scope (do exactly this, no more)

### 1. `src/main.rs` — add warning when auth is disabled

In the HTTP server startup block (where `AppState` is constructed and the
listener is bound), after the `tracing::info!` that logs the bind address,
add a warning when `RECURSIVE_API_KEY` is not set and JWT is not configured:

```rust
// Warn if auth is effectively disabled
let auth_enabled = std::env::var("RECURSIVE_API_KEY").is_ok()
    || std::env::var("RECURSIVE_JWT_SECRET").is_ok();
if !auth_enabled {
    tracing::warn!(
        "HTTP server started with authentication DISABLED. \
         Set RECURSIVE_API_KEY or RECURSIVE_JWT_SECRET to enable auth. \
         Any client with network access can execute commands."
    );
}
```

Place this block immediately after the bind-address log line (search for
`tracing::info!` near the `TcpListener::bind` call in the `serve` subcommand
arm of `main()`).

### 2. Tests

Add a unit test or doc-comment test is NOT required here — this is a
runtime log warning, not testable logic. The acceptance criterion is
visual/manual.

Instead, add a `#[test]` in `src/main.rs` `#[cfg(test)]` that verifies
the auth-check logic (the boolean expression) is correct:

```rust
#[test]
fn auth_disabled_when_no_env_vars() {
    // Can't easily test env vars in parallel tests, so just verify
    // the logic compiles and the condition is reachable.
    let api_key_set = false;
    let jwt_set = false;
    let auth_enabled = api_key_set || jwt_set;
    assert!(!auth_enabled);
}
```

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- A `tracing::warn!` is emitted at startup when neither `RECURSIVE_API_KEY`
  nor `RECURSIVE_JWT_SECRET` env vars are set

## Notes for the agent

- Read `src/main.rs` and search for `TcpListener::bind` or `serve` to find
  the right location for the warning.
- Do NOT add a hard error or refuse to start — this is a warning only.
- Do NOT modify `src/http/auth.rs` or change any auth logic.
- **DO NOT modify** `src/agent.rs`, `src/run_core.rs`, `src/runtime.rs`, `src/llm/`.
- **DO NOT call `exit_plan_mode` or `request_plan_mode`.** You are running
  headless; the plan gate has no reviewer. Just read and edit directly.
