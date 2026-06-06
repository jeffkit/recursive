# Goal 241 — Redact secrets in Config Debug output

**Roadmap**: Arch-review bugfixes (security)

**Design principle check**:
- Implemented as: custom `Debug` impl for `Config` replacing secret fields
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

`Config` derives `Debug`, which means any `{:?}` format call, panic
backtrace, or debug log will print `api_key` in plaintext. This risks
leaking credentials to log aggregators, crash reporters, or other observers.

## Scope (do exactly this, no more)

### 1. `src/config.rs` — replace `#[derive(Debug)]` with a manual `impl`

Remove `Debug` from the derive line:

```rust
// Before:
#[derive(Debug, Clone)]
pub struct Config { ... }

// After:
#[derive(Clone)]
pub struct Config { ... }
```

Add a manual `Debug` impl that prints `[REDACTED]` for `api_key`:

```rust
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("workspace", &self.workspace)
            .field("api_base", &self.api_base)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field("model", &self.model)
            .field("provider_type", &self.provider_type)
            .field("preset", &self.preset)
            .field("max_steps", &self.max_steps)
            .field("temperature", &self.temperature)
            .field("system_prompt", &self.system_prompt)
            .field("retry_max", &self.retry_max)
            .field("retry_initial_backoff_secs", &self.retry_initial_backoff_secs)
            .field("retry_max_backoff_secs", &self.retry_max_backoff_secs)
            .field("shell_timeout_secs", &self.shell_timeout_secs)
            .field("headless", &self.headless)
            .field("memory_summary_limit", &self.memory_summary_limit)
            .field("thinking_budget", &self.thinking_budget)
            .field("session_name", &self.session_name)
            .field("max_budget_usd", &self.max_budget_usd)
            .field("extra_dirs", &self.extra_dirs)
            .field("allow_tools", &self.allow_tools)
            .field("context_window_override", &self.context_window_override)
            .field("subagent_max_depth", &self.subagent_max_depth)
            .finish()
    }
}
```

Make sure all fields of `Config` are listed (read the struct definition
before writing the impl to ensure the list is complete and matches any
newly added fields like `subagent_max_depth`, `allow_bypass_permissions`
if that goal lands first, etc.).

### 2. Tests

Add a unit test in `src/config.rs` `#[cfg(test)]`:

```rust
#[test]
fn debug_redacts_api_key() {
    let c = Config { api_key: Some("sk-secret".into()), ..Config::default_test() };
    let dbg = format!("{c:?}");
    assert!(!dbg.contains("sk-secret"));
    assert!(dbg.contains("REDACTED"));
}
```

(Use whichever test-config helper already exists in that file.)

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `format!("{:?}", config)` does NOT contain the api_key value
- All other fields are still visible in Debug output

## Notes for the agent

- Read `src/config.rs` fully before writing the impl — the field list must
  be complete or clippy will warn about missing fields.
- If `Config` has a `#[allow(dead_code)]` or similar attr on the derive,
  keep those attrs when removing Debug from derive.
- **DO NOT modify** `src/agent.rs`, `src/run_core.rs`, `src/runtime.rs`, `src/llm/`.
- **DO NOT call `exit_plan_mode` or `request_plan_mode`.** You are running
  headless; the plan gate has no reviewer. Just read and edit directly.
