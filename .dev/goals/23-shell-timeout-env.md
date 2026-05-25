# Goal 23 — Configurable `run_shell` timeout via env var

## Why

`RunShell::new()` uses a hardcoded 300-second timeout. Long-running
tasks (`cargo build` from cold cache, large `cargo test` matrices)
have no way to lift that ceiling without recompiling Recursive.

We already have a pattern for exactly this kind of env-driven Config
field: goal-15 plumbed `RECURSIVE_RETRY_MAX` /
`RECURSIVE_RETRY_INITIAL_BACKOFF_SECS` /
`RECURSIVE_RETRY_MAX_BACKOFF_SECS` through `Config` to `OpenAiProvider`.

This goal mirrors that pattern for `RunShell`.

## Scope

Touches: `src/config.rs` and `src/main.rs`.

1. In `src/config.rs`:
   - Add `pub shell_timeout_secs: u64` to the `Config` struct.
   - In `Config::from_env()`, parse `RECURSIVE_SHELL_TIMEOUT_SECS`
     with `.unwrap_or(300)` (matching the current hardcoded default).
   - Update the existing `Default` impl (or wherever `Config`'s
     default fields are set) to include `shell_timeout_secs: 300`.
   - Two new tests in the same file: one for the default, one
     verifying that setting `RECURSIVE_SHELL_TIMEOUT_SECS=42`
     produces `config.shell_timeout_secs == 42`. Use the
     existing env-var test helpers if there's a pattern.

2. In `src/main.rs`'s `build_tools()`:
   - Change `RunShell::new(root)` to
     `RunShell::new(root).with_timeout(Duration::from_secs(config.shell_timeout_secs))`.
   - This requires `build_tools()` to accept the `Config` (or just
     the timeout value) instead of only `&Path`. Pick whichever is
     less invasive — passing the Config is more future-proof but
     touches more call sites; passing just the timeout is local
     but adds another parameter. Either is acceptable.

## Acceptance

- `cargo build` green.
- `cargo test` green (123 baseline + 2 new = 125).
- `cargo clippy --all-targets -- -D warnings` green.
- `cargo fmt --all` clean.

## Notes for the agent

- Read `src/config.rs` first to see how `retry_max` (goal-15) is
  threaded — that is *exactly* the shape you want for this goal.
- Use `apply_patch` for everything. `Config` is small; one or two
  hunks at most.
- This is a **pattern cargo-cult** of goal-15. If the implementation
  takes you past 15 steps, stop and write what's different — most
  likely an `apply_patch` anchor issue, not a design problem.
- **In tests, prefer `.to_string()` over `.into()` for string
  literals.** See AGENTS.md section 5.
