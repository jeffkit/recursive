# Manual edit: llm-client-loopback-noproxy

**Date**: 2026-06-21
**Goal**: Fix 7 failing LLM tests that returned `HTTP 502 Bad Gateway`.
The mock servers in `openai`/`anthropic` tests bind to `127.0.0.1`, but
when the developer has a system `HTTP(S)_PROXY` set, reqwest routes those
loopback requests through the proxy, which can't reach the local listener
and answers `502`.

**Approach (revised)**: This is an *environment* concern, not a product
concern, so it is fixed at the environment layer rather than in code. The
HTTP clients keep honoring the standard proxy convention (reqwest default),
and loopback bypass is expressed via the standard `NO_PROXY` mechanism,
injected for cargo-invoked processes through `.cargo/config.toml [env]`.
This keeps product code free of proxy special-casing and does NOT affect
released binaries (the cargo config doesn't travel with the binary), so
production retains standard proxy behavior.

A first attempt baked a `build_http_client` helper into `src/llm/mod.rs`
that re-implemented a subset of reqwest's env-proxy logic (http/https only,
dropping all_proxy/socks). That was reverted because it coupled product code
to a test-environment concern and risked diverging from reqwest's behavior.

**Files touched**:
- `.cargo/config.toml` (new — sets `NO_PROXY` for `cargo test`/`cargo run`)
**Tests added**: none (existing 7 tests now pass)
**Notes**: `[env]` defaults to non-force, so a developer's own `NO_PROXY`
still wins. `cargo test --workspace`, `clippy -D warnings`, and `fmt` clean.
