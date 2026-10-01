# Journal: fix --no-default-features build (issue #55)

Date: 2026-10-01
Goal: Make `cargo check/clippy --no-default-features --lib` pass for the bare
kernel and all 8 single-feature combos (issue #55, P0, arch-defect series 3/9),
plus a CI feature-matrix guard so it cannot rot again.

## Files touched
- `src/tools/run_background.rs` — new neutral home of
  `pub const DEFAULT_KILL_GRACE_PERIOD` (moved out of `mcp`).
- `src/mcp.rs` — re-exports the constant from `tools::run_background`
  (`pub use`), so all existing `mcp::DEFAULT_KILL_GRACE_PERIOD` users keep
  working.
- `src/acp/session.rs:178` — now references
  `crate::tools::run_background::DEFAULT_KILL_GRACE_PERIOD` instead of
  `crate::mcp::…`; `acp` no longer depends on the `mcp` feature (helps #54).
- `src/tools/registry.rs` (`build_standard_tools_with_transport_opt`) —
  `#[cfg(not(feature = "web_search"))] let _ = (&provider,&key,&jina);`
  consumes the three web-search params when the feature is off (fixes 3
  unused-variable warnings under `-D warnings`).
- `src/session/mod.rs:357` — `epoch_day_to_ymd` gets
  `#[cfg_attr(not(feature = "http"), allow(dead_code))]` (only http handlers
  use it; dead in the bare kernel).
- `.github/workflows/ci.yml` — new `feature-matrix` job: matrix of
  `["", mcp, http, cli, web_fetch, anthropic, skill-hub, web_search]`, each
  running `cargo check --lib --no-default-features [features]` AND
  `cargo clippy --lib --no-default-features [features] -- -D warnings`.

## Tests added
None (build/CI-guard fix; the guard itself is the test surface).

## Notes
- README:38-39 promise is now true again (bare kernel builds, clippy-clean);
  no README change needed.
- Verified locally: all 8 combos `cargo check` + `clippy -D warnings` green;
  full `cargo clippy --all-targets --all-features -D warnings` green;
  `cargo test --workspace` 47 result lines, 0 failures; `cargo fmt --check` ok.
- Long-term follow-ups stay in #53/#54 (make acp/schema optional so the bare
  kernel is actually minimal, not just compilable).
