# Manual edit: cli-http-feature

**Date**: 2026-06-29
**Goal**: Unblock the workspace clippy gate (`--all-features -D warnings`),
which was failing on `recursive-cli` with `unexpected_cfgs` because
`#[cfg(feature = "web_search")]` / `#[cfg(feature = "http")]` gates in
`cli/builder.rs` and `main.rs` referenced features that `recursive-cli` never
declared. The `[dependencies]` line already unconditionally enables
`recursive/web_search` and `recursive/http`, so those gates were also dead
code (always false) — meaning `WebSearch` registration and the entire
`recursive http` subcommand were never compiled.

**Files touched**:
- `crates/recursive-cli/Cargo.toml` — declare `web_search` and `http`
  features and put them in `default` so the cfg gates evaluate to true
  (matching the always-on dependency) and satisfy `unexpected_cfgs`.
- `src/http/mod.rs` — add `pub async fn serve_with_graceful_shutdown(...)`
  that wraps `axum::serve(listener, router).with_graceful_shutdown(shutdown)`,
  so the CLI can serve the API without a direct `axum` dependency.
- `crates/recursive-cli/src/main.rs` — `Cmd::Http` arm now calls
  `recursive::http::serve_with_graceful_shutdown(...)` instead of
  `axum::serve(...)`; also fixed a latent `use of moved value: home` bug
  in the skill-path discovery block (cloned `home` before first move).

**Tests added**: none (no new public logic beyond a thin axum wrapper that
is exercised end-to-end by the existing HTTP suites).

**Notes**:
- Activating the `http` feature exposed two latent compile bugs in the
  `Cmd::Http` arm — the direct `axum::serve` call and the double-move of
  `home` — because that arm had been cfg-gated off (feature never declared)
  since it was authored on 2026-05-26 and thus never compiled. Both are now
  fixed, so `recursive http` is a real, working subcommand.
- `tokio-util` in `recursive-cli` keeps `features = ["rt"]`; `CancellationToken`
  resolves because `tokio-util/sync` is enabled transitively via `recursive`'s
  dependency (feature unification). Unchanged here.
- All three quality gates green: `cargo fmt --all --check`,
  `cargo clippy --workspace --all-targets --all-features -- -D warnings`,
  `cargo test --workspace`.
