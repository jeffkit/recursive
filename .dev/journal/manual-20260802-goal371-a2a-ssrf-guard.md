# Manual edit: goal-371 — SSRF guard for a2a tools (shared url_guard helper)

**Date**: 2026-08-02
**Goal**: Apply the existing SSRF guard (`validate_url` + `is_private_ip`) to the three a2a
tool execute methods, which POST/GET/poll model-supplied URLs with no validation.

## Files touched

- `src/tools/url_guard.rs` — **new** module. `validate_url` + `is_private_ip` moved
  **verbatim** from `src/tools/web_fetch.rs` (including the scheme check, host extraction,
  `localhost`/`metadata.google.internal` blocks, and the v4/v6 range checks). Both are
  `pub(crate)`. Module doc notes the verbatim-move constraint.

- `src/tools/mod.rs`
  - Added `pub(crate) mod url_guard;` between `transport` and `watch_file`. **Unconditional**
    (a2a is not feature-gated; web_fetch is `#[cfg(feature = "web_fetch")]`).

- `src/tools/web_fetch.rs`
  - Removed the inline `validate_url` method and free `is_private_ip` fn; removed
    `use std::net::IpAddr;`.
  - Added `use crate::tools::url_guard::validate_url;`; `execute` now calls the free
    function (`let validated_url = validate_url(url)?;`). Behaviour byte-for-byte unchanged.
  - Tests: `WebFetch::validate_url(...)` → `validate_url(...)`; `mod tests` imports
    `is_private_ip` explicitly (it is no longer module-level). All 20 web_fetch tests pass.

- `src/tools/a2a.rs`
  - **All three tools** (`A2aCallTool` :252, `A2aCardTool` :812, `A2aTaskCheckTool` :959)
    now call `crate::tools::url_guard::validate_url(url)?` immediately after parsing the
    `url` arg, and derive `base` from `validated_url` (`validated_url.trim_end_matches('/')`).
    This covers the streaming path too (`execute_streaming` receives the validated base).
  - The guard runs BEFORE the `prompt`/`task_id` arg checks in `A2aCallTool`/`A2aTaskCheckTool`
    (right after url parse, per the goal), so SSRF rejection is genuinely at the
    args-validation stage.
  - **Test-only escape hatch**: each tool struct gained a `#[cfg(test)] allow_private_urls:
    bool` field + `#[cfg(test)] pub(crate) fn new_unchecked()`. 12 existing mock-server tests
    (they bind `127.0.0.1`, which the guard now rejects) were switched to `new_unchecked()`;
    production `new()` always guards. This was necessary to keep the protocol coverage green
    — without it, `cargo test --workspace` would fail those 12 tests.
  - `missing_prompt_returns_bad_tool_args_error` (the misleading "localhost" test the goal
    flagged): URL changed `http://localhost` → `https://example.com` so it genuinely tests
    the missing-prompt path (with `default()` = guarded). `missing_url_returns_bad_tool_args_error`
    switched to `default()` too (same semantics as `new()`).
  - **New SSRF tests** (3): `a2a_call_rejects_ssrf_targets`, `a2a_card_rejects_ssrf_targets`,
    `a2a_task_check_rejects_ssrf_targets` — each loops over `127.0.0.1`, `169.254.169.254`,
    `10.0.0.1`, `[::1]`, `localhost` and asserts `Error::BadToolArgs` (i.e. rejected at
    args-validation, no socket opened). Constructors use `new()` so the guard is active.

## Tests added

- `src/tools/a2a.rs`: `a2a_call_rejects_ssrf_targets`, `a2a_card_rejects_ssrf_targets`,
  `a2a_task_check_rejects_ssrf_targets`.

## Verification

- `cargo test --workspace` — green (exit 0).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — clean.
- `cargo fmt --all` — clean.
- `rg "validate_url|is_private_ip" src/tools/url_guard.rs` → 3 hits (helpers live there).
- `rg "url_guard::validate_url" src/tools/a2a.rs` → 6 hits (≥3: one per execute method, in
  both `#[cfg(test)]`/`#[cfg(not(test))]` branches).
- a2a suite: 24/24 pass (incl. 3 new SSRF + 12 mock-server protocol tests via `new_unchecked`).
- web_fetch suite: 18 pass / 2 pre-existing `#[ignore]` (SSRF-related mock tests were already
  ignored before this change; the extraction did not change their status).

## Notes / judgment calls

- **Test-only `new_unchecked()` bypass** (deviation beyond the literal goal text): the goal's
  acceptance requires `cargo test --workspace` green, but 12 existing a2a tests hit
  `http://127.0.0.1:{port}` mock servers — after the guard they would all fail with
  `BadToolArgs`. Options considered: DNS names (`lvh.me`/nip.io — flaky, needs internet),
  `#[ignore]`-ing the protocol tests (loses real coverage; web_fetch precedent), global
  test-state flag (race-prone). Chose the per-instance `#[cfg(test)]` field: deterministic,
  no production reachability (field/ctor compile out of non-test builds). Trade-off: the
  public unit structs `A2aCallTool;` became braced structs with a private field — a source-
  breaking change for external code that constructs them as unit literals (nothing in this
  repo does; registry uses `::new()`). Acceptable for an agent crate.
- **Hardcoded `name: "WebFetch"` in `BadToolArgs`**: kept verbatim per the goal ("move it
  verbatim, don't improve it"). Consequence: a2a SSRF rejections report tool name
  `"WebFetch"` in the error. Cosmetic wart — suggest a follow-up to parameterize the name.
- **Redirect bypass not addressed**: a2a's `build_client` has no `redirect::Policy::none()`;
  a public URL could 3xx-redirect to a private host. Goal 372 covers web_fetch; a2a has the
  same hole — out of scope here (noted for a follow-up).
- **`src/providers_cache.rs` has its own separate `validate_url`** for provider endpoints
  (not part of web_fetch's helper) — deliberately untouched.
- **API-shape change**: `A2aCallTool`/`A2aCardTool`/`A2aTaskCheckTool` are re-exported
  publicly (`pub use a2a::{...}`); adding a private field changes the struct shape from unit
  to braced. No internal usage constructs them directly; docs.rs consumers using the unit
  literal would need `::new()`.
