# Manual edit: goal-365 — validate `web_search_provider` at config time (no silent DDG fallback)

**Date**: 2026-08-02
**Goal**: Reject set-but-unknown `web_search_provider` values in `Config::validate_for_agent()`,
so a typo like `RECURSIVE_WEB_SEARCH_PROVIDER=braev` produces a clear startup error instead of
silently degrading the WebSearch tool to the zero-config DDG scrape while ignoring the paid
`RECURSIVE_WEB_SEARCH_API_KEY`.

## Files touched

- `src/config.rs`
  - Added a `#[cfg(feature = "web_search")]` block in `validate_for_agent()` right after the
    existing `provider_type` check (config.rs ~662). When `web_search_provider` is `Some(p)` and
    `crate::tools::web_search::Provider::from_str(p)` returns `None`, returns `Err` naming the
    bad value and listing `brave, tavily, serper, bocha, bing`. `None` (unset) stays valid —
    DDG/Bing zero-config scraping is intentional.
  - Added 3 tests (feature-gated with the same `#[cfg(feature = "web_search")]`):
    `validate_rejects_unknown_web_search_provider` (Some("braev") → Err containing "braev" +
    all valid names), `validate_accepts_known_web_search_provider` (Some("brave") → Ok),
    `validate_accepts_unset_web_search_provider` (None → Ok).
- `src/tools/web_search.rs`
  - `enum Provider` → `pub(crate) enum Provider` and `fn from_str` → `pub(crate) fn from_str`
    (visibility only — the goal's permitted exception). `pub(crate)` keeps the enum out of the
    public API while letting config.rs call the lookup.

## Design decisions

1. **Validation lives in `validate_for_agent()`, not `from_env()`.** Per the goal's trap note,
   `from_env` runs for `recursive config show` / `recursive doctor`; putting the check there
   would make those fail. `validate_for_agent` runs only when the agent is about to start
   (Run/Repl/Resume/Http) — the right time to catch the typo. Its return type is
   `Result<(), String>` (not `crate::error::Result`), so the error is a multi-line `String` in
   the same style as the `provider_type` check, not `Error::Config` as the goal template
   sketched.

2. **Check calls `Provider::from_str` (DRY); only the message lists names.** New providers
   added to the enum are auto-accepted by validation. The "Valid: ..." list in the message is
   hardcoded for readability but verified against the actual `from_str` arms
   (`brave, tavily, serper, bocha, bing`).

3. **`pub(crate)` instead of `pub` for `Provider::from_str`.** clippy's
   `should_implement_trait` fires on a *public* inherent `from_str` returning `Option` (it
   wants a `FromStr` impl). `pub(crate)` makes the method visible to config.rs (same crate)
   without exporting it, so the lint stays silent — no `#[allow]`, no rename, no trait impl,
   no runtime-logic change. An attempted `impl std::str::FromStr` did NOT silence the lint
   (signature mismatch `Option` vs `Result`), so `pub(crate)` was the minimal fix.

4. **Feature-gating.** `pub mod web_search` is behind `#[cfg(feature = "web_search")]`
   (default feature). The validation block and its tests are gated the same way; when the
   feature is off the provider value is inert (registry only wires it under the feature), so
   skipping validation is correct. `cargo build --no-default-features` fails on the base
   commit too (pre-existing `src/acp/session.rs` → `crate::mcp` reference) — not introduced
   here.

## Tests added

- `config::tests::validate_rejects_unknown_web_search_provider`
- `config::tests::validate_accepts_known_web_search_provider`
- `config::tests::validate_accepts_unset_web_search_provider`

## Verification

- `cargo test --workspace` → green (36 suites, 0 failed; lib 2198 + TUI 809, all ok).
- `cargo test --lib "web_search_provider"` → 5 passed (3 new + 2 existing empty-string tests).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` → clean.
- `cargo fmt --all` → clean.
- `git diff --stat` → `src/config.rs` (+84), `src/tools/web_search.rs` (+9/-2) only.
- Manual reasoning: `RECURSIVE_WEB_SEARCH_PROVIDER=braev` now surfaces
  `Error: Unknown web search provider 'braev'. Supported providers: brave, tavily, serper,
  bocha, bing ...` at agent startup — proven by the test asserting `Err` containing `"braev"`.

## Files NOT touched (per scope)

`src/run_core.rs`, `src/kernel.rs`, `src/runtime.rs`, web_search.rs runtime logic
(`WebSearch::load_config` etc.), the DDG/Bing zero-config fallback (still active when the
provider is unset), `.dev/flows/`, `Cargo.toml`.
