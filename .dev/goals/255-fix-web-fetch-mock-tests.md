# Goal 255 — Fix web_fetch mock server tests hanging after SSRF protection

**Roadmap**: Infrastructure — fix hanging tests that block self-improve loop

**Design principle check**:
- Implemented as: fix two unit tests in `src/tools/web_fetch.rs` that hang indefinitely
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

`src/tools/web_fetch.rs` has two tests that spin up a `std::net::TcpListener`
mock server and call `WebFetch::execute()` with a `127.0.0.1` URL:

- `test_c_body_exceeds_max_bytes` (line 516)
- `web_fetch_tool_on_mock_server` (line 559)

When the SSRF protection (added in commit 30ba548) was introduced, it started
blocking all loopback/127.x requests. Now these tests call `validate_url()`
which rejects `127.0.0.1` and returns early — the HTTP request is never made,
so the mock server's `listener.accept()` waits forever, and `handle.join()`
never returns. The test hangs until the OS kills it (or the CI times out).

This causes `cargo test --workspace` to hang indefinitely, blocking the entire
self-improve loop — every goal times out and rolls back even if the code change
itself was correct.

## Scope (do exactly this, no more)

### 1. `src/tools/web_fetch.rs` — fix the two hanging tests

Read both tests fully (lines 515–590) before making changes.

**Option A — test the internal `fetch_url` function directly** (preferred):

If `WebFetch` has an internal `fetch_url` or similar method that bypasses
`validate_url`, call that directly in the test. This keeps the mock server
pattern intact and tests the actual HTTP logic.

**Option B — test `validate_url` and HTTP separately**:

Split each test into two parts:
1. A sync test that asserts `validate_url("http://127.0.0.1:PORT")` returns
   `Err` (SSRF blocked) — no network needed.
2. A separate test that calls the internal HTTP fetch function directly on a
   `127.0.0.1` address, bypassing `validate_url`.

**Option C — simplest: mark as `#[ignore]` with a comment**:

If the internal fetch function is not easily accessible, add `#[ignore]` to
both tests with a comment explaining why:

```rust
#[tokio::test]
#[ignore = "mock server uses 127.0.0.1 which is blocked by SSRF protection; test the HTTP layer separately"]
async fn test_c_body_exceeds_max_bytes() {
```

Choose whichever option requires the fewest changes. Read the `WebFetch`
struct and its methods first to determine if Option A or B is feasible.
If not, use Option C.

### 2. Verify `cargo test` completes

After the fix, confirm that `cargo test -p recursive-agent` (or
`cargo test --workspace`) runs to completion without hanging. The
`web_fetch` tests should either pass or be skipped (ignored), not hang.

### 3. Tests

No new tests needed beyond the fix itself. The goal is to stop the hang.

## Acceptance

- `cargo test --workspace` runs to completion (no infinite hang)
- The two previously-hanging tests either pass or are `#[ignore]`-d with
  a clear comment
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean

## Notes for the agent

- Read `src/tools/web_fetch.rs` lines 430–620 to understand both tests and
  the `WebFetch` struct before deciding on an approach.
- The SSRF check is in `validate_url()` called at line ~272 inside `execute()`.
- Do NOT remove or weaken the SSRF protection in `validate_url`. Fix the
  tests, not the protection.
- Do NOT modify any file other than `src/tools/web_fetch.rs`.
- After editing, run `cargo test -p recursive-agent 2>&1 | tail -20` to
  confirm tests complete. If it finishes (even with failures), the hang is
  fixed — report what you see.
- **DO NOT call `exit_plan_mode` or `request_plan_mode`.** Running headless.
