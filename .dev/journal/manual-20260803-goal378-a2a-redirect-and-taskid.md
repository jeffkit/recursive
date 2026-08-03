# Manual edit: Goal 378 — harden a2a tools (redirect SSRF + async task.id shell injection)

**Date**: 2026-08-03
**Goal**: Goal 378 — Harden a2a tools: disable redirects (SSRF) + sanitize async-mode
task.id (shell injection)

## What changed

All changes are confined to `src/tools/a2a.rs`. No kernel / run-loop / invariant
changes, no new deps.

### 1. Redirects disabled on the shared client (`build_client`)

`A2aCallTool::build_client()` now sets
`.redirect(reqwest::redirect::Policy::none())` (same as `web_fetch.rs:43`), with a
comment explaining the Goal-372 SSRF rationale: `validate_url` checks only the initial
URL, so the default follow-up-to-10-redirects policy would let a public A2A server
302 a request to `169.254.169.254` / `127.0.0.1` after the guard passed. `connect_timeout`
was already present; only the redirect policy was missing.

### 2. Explicit 3xx rejection at every response layer

Added a `redirect_error(name, resp, status)` helper that surfaces a redirect as
`Error::Tool { message: "A2A server returned redirect (<code>) to <location>" }`
(never followed). Wired into all four entry points (5 status-check sites):

- `POST /message:send` in `A2aCallTool::execute` (was ~:285)
- `GET /tasks/{id}` poll loop — moved into the new `poll_task_to_completion` helper
- `POST /message:stream` in `execute_streaming`
- `GET /.well-known/agent.json` in `A2aCardTool::execute`
- `GET /tasks/{task_id}` in `A2aTaskCheckTool::execute`

All five go through `A2aCallTool::build_client()` (card + task_check reuse it), so the
single client change covers every path.

### 3. Async-mode `task.id` whitelist + sync fallback

- New `is_safe_task_id(id)` — strict whitelist `^[A-Za-z0-9._-]+$` (non-empty, ASCII
  alnum + `.` `_` `-`). No shell-escaping / URL-encoding as a substitute; whitelist only.
- In the `use_async` branch of `A2aCallTool::execute`, an unsafe id now falls back to
  **synchronous polling** (preferred per goal notes) and returns the plain result with a
  `WARNING: A2A server returned unsafe task.id ...` line — it never builds a `poll_cmd`
  containing the unvalidated id. Well-formed ids keep the existing async script
  byte-identical.
- The synchronous poll loop was extracted verbatim into `poll_task_to_completion(...)`,
  shared by the normal sync path and the unsafe-id fallback (behavior byte-identical:
  same timeouts, same poll cadence, same result strings).

### 4. Tests (in `src/tools/a2a.rs` `#[cfg(test)]` module)

- `redirect_response_is_rejected_without_following` — raw-TCP mock (same stubbing
  pattern the file already uses) returns `302 Found` with `Location` pointing at a
  second counting mock; asserts the tool returns an `Error::Tool` mentioning
  `redirect` / `302` / the target address, and that the Location target's accept
  counter stays 0 (never fetched).
- `async_mode_unsafe_task_id_falls_back_to_sync` — server returns
  `task.id = "\"; rm -rf /; echo \""`; asserts the call falls back (WARNING present,
  no `curl` poll script emitted; an `Err` from URL building is also accepted as a
  no-injection outcome).
- `async_mode_safe_task_id_produces_poll_script` — `task.id = "t-123_abc.xyz"` still
  yields `TASK_ID: t-123_abc.xyz` + `curl` script + `tasks/t-123_abc.xyz` as before.

## Files touched

- `src/tools/a2a.rs` (only file)

## Tests added

3 new tests in `src/tools/a2a.rs::tests` (see above). Verified:
- `cargo test --manifest-path Cargo.toml --lib a2a` — 27 passed (incl. 3 new).
- `cargo test --workspace` — all green (2221 lib + 811 tui + all integration suites).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — clean.
- `cargo fmt --all` — clean.
- `rg "Policy::none" src/tools/a2a.rs` → ≥1 hit (client config, line ~183).
- `rg "reject.*task.id|sanitize|\[A-Za-z0-9._-\]" src/tools/a2a.rs` → ≥1 hit in the
  async branch / whitelist helper.

## Notes

- **Streaming path verified**: `execute_streaming` uses the same shared client from
  `build_client()` and now rejects 3xx before entering the SSE read loop.
- **Poll loop redirect**: the extracted `poll_task_to_completion` includes the same
  `is_redirection()` rejection, so a 3xx during a synchronous poll is also surfaced
  (previously it would have been treated as a JSON error or followed).
- **Fallback choice**: followed the goal's preference — unsafe `task.id` falls back to
  synchronous polling with a warning rather than erroring the whole call. The warning
  echoes the raw id via `{:?}` (debug quoting) so the model can see what was rejected
  without it being re-usable as shell.
- `Error::Tool` is used for redirects (matching the goal's stated wording and
  web_fetch's approach), while non-3xx HTTP errors keep their existing
  `Ok("ERROR: HTTP ...")` convention — no behavior change outside the redirect case.
