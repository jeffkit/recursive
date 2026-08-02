# Goal 372 — Block web_fetch SSRF redirect bypass (validate-once, follow-10-redirects)

**Roadmap**: Tools / security — `validate_url` runs once; a 302 to a private IP bypasses it

**Design principle check**:
- Implemented as: set `reqwest::redirect::Policy::none()` on the web_fetch client builder
  and surface non-2xx (including 3xx) as an error, OR re-validate the final URL after
  redirects. Plus a regression test with a mock server.
- ❌ Does NOT touch the agent kernel, run loop, or any invariant. WebFetch behaviour for
  *direct* URLs (no redirect) is unchanged; only the redirect-following path is hardened.
- No new deps (`reqwest::redirect::Policy` is in the reqwest already in use).

## Why (the redirect bypass, with evidence)

`src/tools/web_fetch.rs:32` builds the client:
```rust
let client = Client::builder()
    .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
    .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
    .user_agent(...)
    .build();
```

**No `.redirect(...)` is set.** Verified 2026-08-02:
`grep -n 'redirect\|RedirectPolicy\|Policy' src/tools/web_fetch.rs` returns **0 hits**.
reqwest's default is `Policy::default()` which **follows up to 10 redirects**.

`validate_url` (`src/tools/web_fetch.rs:51`) runs **once**, on the *input* URL only. So:
- Attacker serves `https://attacker.example/redirect` (passes `validate_url` — it's public).
- That endpoint returns `302 Location: http://169.254.169.254/latest/meta-data/iam/...`.
- reqwest follows the redirect and fetches the IMDS endpoint — **no re-validation**.

The existing test `validate_url_blocks_ssrf_targets` only pins the *initial-URL* check.
Two `#[ignore]`d tests at `src/tools/web_fetch.rs:545` (`test_c_body_exceeds_max_bytes`)
and `:591` (`web_fetch_tool_on_mock_server`) are ignored precisely because the SSRF guard
blocks the mock-server 127.0.0.1 — they document the gap but don't cover the redirect path.

## Scope (do exactly this, no more)

### 1. Disable redirect-following (preferred) OR re-validate post-redirect

**Option A (preferred — simplest, safest):** set `Policy::none()` and treat 3xx as an error
the caller sees (web_fetch is a one-shot fetch tool; redirect chains aren't part of its
contract). In `WebFetch::new()`:
```rust
let client = Client::builder()
    .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
    .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
    .user_agent(format!("recursive-agent/{}", env!("CARGO_PKG_VERSION")))
    .redirect(reqwest::redirect::Policy::none())   // NEW: don't follow; SSRF-safe
    .build();
```
Then in `execute`, a 3xx response is surfaced as an error (don't silently treat 3xx as
failure — return a clear `Error` like "WebFetch does not follow redirects; got 3xx to
<location>"). Read the existing response-status handling in `execute` to see where to add
this.

**Option B (if redirects are actually needed by some caller):** keep follow-on but, after
`client.execute(req).await`, **re-validate `response.url()`** with `validate_url` — i.e.
check the *final* resolved URL's host is not private. If it is, error out without reading
the body.

Pick A unless you find a real caller that depends on redirect-following (grep for usages
of `WebFetch` — likely only the tool registry). Document the choice in the journal.

### 2. Regression test (`src/tools/web_fetch.rs` `#[cfg(test)]` module)

Add a test using a local mock server (e.g. `wiremock` if already a dev-dep, or the same
mock harness the ignored tests at `:545`/`:591` intended to use):
- Mock server bound to a **non-private** test address (the existing tests used 127.0.0.1
  which the SSRF guard blocks — you need the *guard to let the request through* to test
  the redirect path. Options: (a) make `validate_url` injectable / bypassable in tests via
  a seam, or (b) bind the mock to a public-looking address via `/etc/hosts` in the test,
  or (c) refactor `execute` to take the `reqwest::Client` so the test can inject one with
  a permissive policy for the mock but assert the *production* builder uses `Policy::none`).
- The mock returns `302 Location: http://169.254.169.254/`.
- Assert `execute()` returns `Err(...)` and does **not** issue a request to 169.254.169.254
  (under Option A: because Policy::none stops at the 3xx; under Option B: because the
  re-validation rejects it).

If driving a full mock-server test is too heavy (the ignored tests suggest it is — they
were disabled for exactly this friction), the **minimum viable** test is to assert the
client builder uses `Policy::none()`. This requires a small refactor: extract the builder
into a `fn build_client() -> reqwest::ClientBuilder` and test that it's configured with no
redirect — but that's awkward to assert directly. **Prefer the behaviour test if at all
possible**; the ignored tests already prove the harness can build a mock server (the
ignoring was due to the SSRF guard blocking 127.0.0.1, which your test-setup seam must
solve).

Read the ignored tests at `:545` and `:591` first — they may be un-ignorable once you add
the seam, killing two birds (covering the truncation path AND the redirect path).

## Files NOT to touch

- `src/run_core.rs`, `src/kernel.rs`, `src/runtime.rs` — unrelated.
- `validate_url` / `is_private_ip` logic itself — Goal 371 moves these to a shared helper;
  **coordinate with 371**: if 371 has landed, import from `tools::url_guard`; if not,
  leave them in `web_fetch.rs`. Either way, don't change the validation rules here.
- `src/tools/a2a.rs` — a2a has the same redirect hole, but it's out of scope for this goal
  (note it in the journal; 371 covers a2a's URL-guard, redirect-policy-for-a2a is a
  follow-up).
- Other tools. `.dev/flows/`.

## Acceptance

- `cargo test --workspace` green.
- `cargo clippy --workspace --all-targets --all-targets -- -D warnings` clean.
- `cargo fmt --all` clean.
- Grep: `rg "redirect\(reqwest::redirect::Policy" src/tools/web_fetch.rs` returns **1 hit**
  (Option A) OR `rg "response\.url\(\)" src/tools/web_fetch.rs` shows post-redirect
  re-validation (Option B).
- The new test asserts a 302-to-private-IP does NOT result in fetching the private IP.
- Existing `validate_url_blocks_ssrf_targets` test still passes (direct-URL guard intact).

## Notes for the agent (traps)

- **Default reqwest follows 10 redirects.** This is the entire bug. `Policy::none()` is
  the fix; don't reach for `Policy::limited(n)` (that still follows, just fewer).
- **3xx is not necessarily an error to reqwest by default.** Under `Policy::none()`, a 3xx
  comes back as a normal `Response` with that status — you must check `response.status()`
  and decide. Don't assume reqwest errors on 3xx.
- **The ignored tests at `:545`/`:591` are a hint, not a spec.** They were ignored because
  the SSRF guard blocks 127.0.0.1 — your test needs to either bypass the guard for the
  *initial* request (to reach the redirect logic) or use a non-private mock host. A clean
  way: refactor `execute` to split `validate_and_fetch(client, url)` so the test injects a
  client whose policy is permissive but asserts the production path uses `Policy::none()`.
- **Coordinate with Goal 371.** 371 extracts `validate_url` into `tools::url_guard`. If
  371 lands first, your import changes. If you land first, 371 will adjust. Either order
  works — just check the current state of `validate_url`'s location before editing.
- **Don't expand to a2a.** a2a has the identical redirect hole, but fixing it there is a
  follow-up (after 371 adds the URL guard). Note it in the journal and move on.
- **Network-test discipline** (per `.dev/AGENTS.md` invariant guidance): if the test makes
  any real outbound call, wrap it in `tokio::time::timeout(...)` and use explicit
  `.connect_timeout`. Don't let a flaky mock hang the suite.
