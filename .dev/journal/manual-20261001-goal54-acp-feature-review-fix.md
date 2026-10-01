# Review-fix: ACP feature gate follow-ups (goal #54)

Date: 2026-10-01
Goal: Address NEEDS_FIX from independent review of goal #54 (acp feature gate).

## Round 2 (2026-10-01, second NEEDS_FIX)

Reviewer reproduced exit 101 on the two new CI guard steps under CI's own
`RUSTFLAGS=-D warnings`: the round-1 compile fixes had been lost in the run
resume. Restored both (parity with sibling attempt `3bd9f5d`):

- `src/session/mod.rs`: `#[allow(dead_code)]` + comment on
  `epoch_day_to_ymd` — sole non-test caller is `http` handlers, dead in
  minimal builds.
- `src/tools/registry.rs`: `#[cfg(not(feature = "web_search"))]` block
  no-op-consuming `web_search_provider` / `web_search_api_key` /
  `web_search_jina_key` so the params stay in the signature regardless of
  feature set.

Verified with CI's exact warning policy:
- `RUSTFLAGS="-D warnings" cargo check --no-default-features --lib` → exit 0
- `RUSTFLAGS="-D warnings" cargo check --no-default-features --features http --lib` → exit 0
- `RUSTFLAGS="-D warnings" cargo check --no-default-features --features acp --lib` → exit 0
- `RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets --all-features -- -D warnings` → clean
- `cargo test --workspace` → 57 suites, 0 failed (3754 passed)
- `cargo fmt --all --check` → clean

Note: one earlier `cargo test --workspace` run showed flaky failures in
`tools::shell::tests::timeout_kills_child_process` (marker file race, host
load) and `session_host::tests::evict_idle_does_not_block_reads_while_closing`
(timing assertion); both pass in isolation and in the final full run. Neither
is touched by this branch (files unchanged vs `bc63d74`).


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
