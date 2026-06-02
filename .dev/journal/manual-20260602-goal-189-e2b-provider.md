# Manual edit: goal-189-e2b-provider

**Date**: 2026-06-02
**Goal**: Goal 189 — CloudRuntime: E2B MicroVM ToolProvider (L3 硬件级沙盒)
**Files touched**:
- `src/tools/e2b_provider.rs` (new)
- `src/tools/mod.rs` (add `#[cfg(feature = "e2b-sandbox")] pub mod e2b_provider;`)

**Tests added**:
- `src/tools/e2b_provider.rs` — unit tests for `E2bConfig::from_env()` and live smoke test
  (`create_and_exec_in_sandbox`, gated on `RECURSIVE_E2B_API_KEY` env var)

**Notes**:
- Replaced `reqwest::multipart` form upload with raw bytes + query param to avoid requiring the
  `multipart` reqwest feature. This is compatible with E2B's file upload REST endpoint.
- `E2bToolSetProvider` lazily creates a single sandbox per provider instance (Mutex<Option<E2bSandbox>>)
  and reuses it across `execute()` calls to amortize sandbox startup cost.
- `E2bSandbox::drop` spawns a background DELETE request to clean up the sandbox on the E2B side.
- The module is feature-gated by `e2b-sandbox` in `Cargo.toml` and `src/tools/mod.rs`.
- All three quality gates pass: `cargo test --features e2b-sandbox`, `cargo clippy ... -D warnings`,
  `cargo fmt --all`.
