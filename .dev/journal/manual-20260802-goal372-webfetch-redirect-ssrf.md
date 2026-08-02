# Manual edit: goal-372 — Block web_fetch SSRF redirect bypass (validate-once, follow-10-redirects)

**Date**: 2026-08-02
**Goal**: Close the SSRF redirect bypass in `web_fetch`: `validate_url` runs once on the
initial URL, and reqwest's default redirect policy (follow up to 10) would let a public
URL 302-redirect the fetch to a private host (e.g. AWS IMDS `169.254.169.254`) with **no
re-validation**.

## Decision: Option A (`Policy::none()`), not Option B (post-redirect re-validation)

Grep of every `WebFetch` usage shows no caller depends on redirect-following:
- `crates/recursive-cli/src/cli/builder.rs:105` — `register(Arc::new(WebFetch::new()))` only.
- `tests/deferred_tool_loading.rs` — constructs `WebFetch::new()` but only checks
  deferred-tool registry behaviour, never exercises redirects.
- `web_fetch` is a one-shot HTTP GET tool; redirect chains aren't part of its contract.

So **Option A** (disable redirect-following; surface 3xx as a clear error) is the correct,
minimal fix. It also means no `response.url()` re-validation path is needed (that's the
Option B fallback and would require reading the response before knowing the final URL).

## Files touched

- `src/tools/web_fetch.rs`
  - `WebFetch::new()` (builder, ~line 37-43): added `.redirect(reqwest::redirect::Policy::none())`
    with a comment explaining why (validate-once + follow-N-redirects = SSRF bypass).
  - Split `execute()` → `execute()` + new `async fn fetch(&self, validated_url: &str, max_bytes: usize)`
    (~line 57-133). `execute` keeps arg parsing + `validate_url` + max_bytes, then delegates
    to `fetch`. `fetch` holds the HTTP GET, status handling, body read, truncation, and
    HTML→markdown. This is the test seam the goal asked for: tests can drive the HTTP path
    against a 127.0.0.1 mock without tripping the SSRF guard on the *initial* request.
  - In `fetch`, added an explicit `status.is_redirection()` branch **before** the generic
    `!status.is_success()` check: under `Policy::none()` a 3xx is a *normal* response, so we
    must surface it ourselves. Returns
    `"WebFetch does not follow redirects; got HTTP {status} (Location: {location})"`
    (Location read from the response header; `<none>` if absent).

## Tests added / enabled

- NEW `web_fetch_does_not_follow_redirect_to_private_ip` (regression):
  - Raw `TcpListener` thread mock on `127.0.0.1:0` returns
    `302 Location: http://169.254.169.254/latest/meta-data/`.
  - Drives `WebFetch::new().fetch("http://{addr}/redirect", DEFAULT_MAX_BYTES)` (production
    client → exercises `Policy::none()`), wrapped in `tokio::time::timeout(10s)` per network
    discipline.
  - Asserts the result is an **error** whose message contains "does not follow redirects",
    "302", and "169.254.169.254" — i.e. the 3xx was surfaced, NOT a body read from the
    private IP and NOT a connection error to it.
  - **Verified it fails on old code**: removed the `.redirect(...)` line, re-ran → the
    client followed the 302 to 169.254.169.254 and the environment returned a real
    `HTTP 502` response (proving an actual request hit the private-IP path); the test
    failed on the "does not follow redirects" assertion. Restored the line → passes.
- UN-IGNORED the two previously `#[ignore]`d tests by switching them from `execute(json!({url: "http://127.0.0.1..."}))` to the new `fetch` seam (their ignore reason — "SSRF guard blocks 127.0.0.1 before HTTP request" — is exactly what the seam solves):
  - `test_c_body_exceeds_max_bytes` (truncation path, max_bytes=50 / 200-byte body).
  - `web_fetch_tool_on_mock_server` (basic full-tool fetch path).
  - Both wrapped in `tokio::time::timeout(10s)`.

## Verification

- `cargo fmt --all` — clean.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — clean.
- `cargo test --workspace` — green: lib suite 2211 passed (incl. all 21 `tools::web_fetch`
  tests, 0 ignored — the two former `#[ignore]`s now run), all other suites 0 failed.
- Acceptance greps:
  - `rg "redirect\(reqwest::redirect::Policy" src/tools/web_fetch.rs` → **1 hit** (line 43).
  - `validate_url_blocks_ssrf_targets` still passes (direct-URL guard intact).

## Notes / judgment calls

- **Test seam choice.** The goal offered (a) injectable `validate_url`, (b) `/etc/hosts`
  public host, (c) injectable client. I chose the `fetch`-split seam (a variant of (c)):
  production `execute` still validates before any socket opens; tests bypass *only* the
  initial-URL guard to reach the redirect logic, while `WebFetch::new()` (the production
  client) is what actually performs the request — so the redirect policy under test is the
  real one. `validate_url_blocks_ssrf_targets` continues to pin the initial-URL guard.
- **a2a has the identical redirect hole and is OUT of scope** (goal explicitly says so).
  `src/tools/a2a.rs` `build_client()` sets timeouts but no redirect policy; after Goal 371
  it validates the initial URL but would follow a 302 to a private host the same way.
  Fixing a2a's redirect policy is a follow-up (the 371 journal already flagged it).
- **3xx was not previously an error in a meaningful way.** The old `!status.is_success()`
  check *did* reject 3xx when it saw one — but under the default policy reqwest followed
  the redirect first, so the 3xx was never seen. The new explicit `is_redirection()` branch
  exists to produce the clear "does not follow redirects" error instead of a generic
  `HTTP 302: <body>` (which would be misleading now that 3xx is the terminal outcome).
- **No new deps.** `reqwest::redirect::Policy::none()` is in the reqwest already in use;
  tests reuse the existing raw-`TcpListener` mock pattern from the file (no wiremock/mockito
  needed).
- **`unwrap_or("<none>")`** in the Location read is a non-panicking default (invariant #5
  satisfied; the crate's `#![deny(clippy::unwrap_used, clippy::expect_used)]` is
  test-exempt anyway).

## E2E gate round — fixed an environment failure, not a code failure

The first `sh .dev/scripts/e2e-gate.sh` run failed with `smoke-01: write_file produced
smoke.txt does not exist`. Investigation showed the smoke suite doesn't touch WebFetch at
all (write_file/read_file + aimock replay), so this was **not** a regression from Goal 372.

Root cause chain (three layers of stale state, all from the documented "interrupted gate"
failure mode in AGENTS.md):
1. The flow's first gate invocation was killed mid-flight, stranding the `argusai-wt-06a737f`
   mcp2cli daemon + `e2e/.argusai/history.db` (the documented SESSION_EXISTS trap).
2. After the daemon was stopped and `.argusai` removed, `argus-setup` failed with
   `docker: invalid reference format` because a stale `recursive-e2e` container (Created
   state) from the killed run blocked the container name (name conflict).
3. After removing that container, `argus-setup` failed with
   `network argusai-wt-06a737f-network not found` — the **real root cause**:
   `docker network create` failed with
   **`all predefined address pools have been fully subnetted`**. Docker had accumulated
   **29 orphaned `argusai-wt-*-network` bridge networks** (zero attached containers) from
   previous worktree e2e runs; the gate's `argus-clean` removes containers but leaves the
   network behind, and `argusai-core`'s `ensureNetwork` swallows the create error
   (`catch { // Network may already exist }`), so the failure is silent until the
   subsequent `docker run --network …` reports "network not found".

Fix applied (host-level, no source change): removed all 29 orphaned `argusai-wt-*-network`
networks (`docker network ls --filter name=argusai` → `docker network rm` each), removed
the stale `recursive-e2e` container, stopped the daemon, removed `.argusai`, then re-ran
the gate → **`[e2e-gate] smoke PASSED ✓`** (exit 0). Post-run self-clean verified (no
containers/sessions stranded; removed the empty leftover network to avoid re-accumulating
the pool exhaustion).

**Note for future runs**: this failure mode is now documented in AGENTS.md (Known
self-improve failure modes #5). If a gate run reports a smoke case failing with "file does
not exist" but the suite doesn't touch the changed code, check `docker network ls` for an
accumulated `argusai-wt-*` list before suspecting the code change.
