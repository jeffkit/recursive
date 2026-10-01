# Review-fix: ACP feature gate follow-ups (goal #54)

Date: 2026-10-01
Goal: Address NEEDS_FIX from independent review of goal #54 (acp feature gate).

## Files touched
- `Cargo.toml`: `acp = ["dep:agent-client-protocol-schema", "mcp"]` — declares the
  mcp dependency, since `src/acp/session.rs:178` references
  `crate::mcp::DEFAULT_KILL_GRACE_PERIOD`. `--no-default-features --features acp`
  now compiles.
- `crates/recursive-cli/Cargo.toml`: added `"acp"` to CLI crate's `default`, so
  the `#[cfg(feature = "acp")]` gates in `main.rs` evaluate true in a default
  build — restores the `recursive acp` subcommand (and the Docker/e2e image).

## Tests added
- None (feature-plumbing only); verified empirically:
  - `cargo check -p recursive-agent --no-default-features --features acp` ✓
  - `cargo build -p recursive-cli` → `./target/debug/recursive acp --help` ✓
  - `cargo test --workspace` (57 suites, 0 failed) ✓
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings` ✓
  - `cargo fmt --all` ✓

## Notes
Reviewer Finding #1 root cause: forwarding features only satisfy the
`[dependencies]` `features` list; the CLI crate's own `default` is what makes
in-crate `#[cfg(feature = ...)]` gates true in a default build (documented at
crates/recursive-cli/Cargo.toml lines 38–45). Finding #2 fixed via feature
dependency rather than a CI-only guard, so `acp` is independently buildable.
