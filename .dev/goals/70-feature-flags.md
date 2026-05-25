# Goal 70 — Feature Flags (optional compilation)

**Roadmap**: Phase 7.4 — Feature flags

**Design principle check**:
- Implemented as: Cargo.toml feature configuration + `#[cfg(feature)]`
  guards. No new code, only conditional compilation.
- Does NOT modify agent loop logic.

## Why

Not every user needs MCP, web_fetch, or the Anthropic provider. Feature
flags let users compile only what they need, reducing binary size and
dependencies. This is standard practice for Rust libraries before
crates.io publish.

## Scope (do exactly this, no more)

### 1. `Cargo.toml` — define features

```toml
[features]
default = ["mcp", "web_fetch", "anthropic"]
mcp = []
web_fetch = ["dep:reqwest"]  # or however web_fetch's deps are gated
anthropic = []
```

Features to gate:
- `mcp` — all MCP client/server code in `src/mcp.rs`
- `web_fetch` — the web_fetch tool in `src/tools/web_fetch.rs`
- `anthropic` — the Anthropic provider in `src/llm/anthropic.rs`

### 2. Apply `#[cfg(feature = "...")]` guards

For each feature:
- Gate the module declaration in `src/lib.rs`:
  ```rust
  #[cfg(feature = "mcp")]
  pub mod mcp;
  ```
- Gate tool registration in `src/tools/mod.rs` or wherever tools are
  registered
- Gate provider registration in `src/llm/mod.rs`
- Gate re-exports in `src/lib.rs`
- Gate CLI flags in `src/main.rs` (e.g. `--mcp-config` only with "mcp")

### 3. Verify compilation without features

Test that these all compile:
```bash
cargo build --no-default-features
cargo build --features mcp
cargo build --features web_fetch
cargo build --features anthropic
cargo build  # all defaults
```

### 4. Update Cargo.toml metadata

Ensure the `[package]` section includes:
- Correct `description`
- `keywords` and `categories` for crates.io
- `documentation` pointing to docs.rs
- `repository` URL

### 5. Tests

- All existing tests pass with default features
- `cargo test --no-default-features` passes (core tests only)
- No dead code warnings when features are disabled

## Acceptance

- `cargo build --no-default-features` compiles
- `cargo build` (all features) compiles
- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- No unnecessary dependencies pulled in when features disabled

## Notes for the agent

- Read `Cargo.toml` for current dependencies.
- `reqwest` is used by web_fetch and MCP HTTP transport. It should be
  gated behind the appropriate feature(s).
- Be careful with `src/main.rs` — the binary always needs to compile.
  Use `#[cfg]` to conditionally include CLI args and tool registrations.
- The `default` feature should include everything (backward compatible).
- Watch out for cross-feature dependencies: if `mcp` needs `reqwest`,
  add `mcp = ["dep:reqwest"]` in features.
- Run `cargo build --no-default-features 2>&1` to find what breaks.
