# acp feature gate (#54)

- Date: 2026-10-01
- Goal: #54 — make `src/acp` + `agent-client-protocol-schema` optional behind an `acp` feature (kept in `default`; value is "can be turned off", not "off by default"). Prereq #53 (ToolKind moved to `src/tools/tool_kind.rs`) already landed.
- Files touched:
  - `Cargo.toml`: `agent-client-protocol-schema` → `optional = true`; features `acp = ["dep:agent-client-protocol-schema"]`, added to `default`.
  - `src/lib.rs`: `#[cfg(feature = "acp")] pub mod acp;`
  - `crates/recursive-cli/Cargo.toml`: recursive dep features += `"acp"`; forwarding feature `acp = ["recursive/acp"]`.
  - `crates/recursive-cli/src/main.rs`: `#[cfg(feature = "acp")]` on `Cmd::Acp` variant + match arm (same pattern as `Cmd::Http`).
  - `crates/recursive-tui`: untouched (zero refs to `recursive::acp`); verified builds without `acp`.
  - `.github/workflows/ci.yml`: two guards — `cargo check --no-default-features --lib` and `--features http --lib`.
  - `.dev/scripts/agent-mutants.sh`: FEATURES += `acp` (keep mutant coverage).
  - Dockerfile: no change (builds via `-p recursive-cli --features http`; cli dep line now carries `acp`).
- Tests added: CI feature-combination checks only (gate is declarative; no new unit tests).
- Notes:
  - `src/tools/client_fs.rs` uses `crate::tools::tool_kind::ToolKind` (post-#53) — no ACP cfg needed in the tools layer; its `tracing::info!(target: "recursive::acp", ...)` log targets are just strings.
  - `cargo check --no-default-features --lib` shows pre-existing warnings (unused vars in `registry.rs`, dead `epoch_day_to_ymd`) — unrelated to this change, left as-is.
  - Verified: `cargo test --workspace` green, `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean, `cargo fmt --all` applied.
