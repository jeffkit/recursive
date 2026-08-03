# Goal 378 — Harden a2a tools: disable redirects (SSRF) + sanitize async-mode task.id (shell injection)

**Roadmap**: Tools / security — the a2a tool family re-opens the Goal-372 redirect SSRF
hole; async-mode poll script embeds server-controlled task.id into a shell command

**Design principle check**:
- Implemented as: a client-config change (`Policy::none()` on the shared reqwest client,
  matching `web_fetch.rs:43`) + a 3xx-handling tweak at the response layer + a
  task.id whitelist check in the async-mode branch. No new deps. No kernel/run-loop
  changes.
- ❌ Does NOT touch the agent kernel, run loop, or any invariant.

## Why (both verified 2026-08-03 by reading the code)

**1. Redirect SSRF.** `src/tools/a2a.rs:171-176` `build_client()` sets timeouts only —
no `.redirect(reqwest::redirect::Policy::none())`. `web_fetch.rs:37-43` sets
`Policy::none()` with a comment explaining exactly why: `validate_url` checks only the
initial URL; following redirects would let a public A2A server 302 a request
(`POST /message:send` at `:314`, `GET /tasks/{id}` at `:397`, `POST /message:stream` at
`:543`, `GET /.well-known/agent.json` at `:844`) to `169.254.169.254` / `127.0.0.1` after
the guard passed, and the internal response comes back to the model. Goal 372 closed
this for web_fetch; a2a never got it.

**2. Shell injection in async mode.** `src/tools/a2a.rs:358-369`: when a call is made
with `async_mode: true`, the tool builds a `poll_cmd` shell one-liner that embeds
`{task_id}` (from the A2A server's `task.id` field) directly inside single quotes:
`r=$(curl -sf '{base}/tasks/{task_id}' ...)` and `echo "A2A task {task_id} finished: $s"`.
A malicious/buggy A2A server returning `task.id` containing `'`, `;`, `` ` ``, `$()`
yields command injection when the model passes that script to `run_background`. The
generated curl also bypasses the SSRF guard entirely (raw URL from string concatenation).

## Scope (do exactly this, no more)

### 1. `build_client()`: disable redirects

In `src/tools/a2a.rs:171-176`, add to the client builder:
`.redirect(reqwest::redirect::Policy::none())` — same as `web_fetch.rs:43`. Also set a
reasonable `connect_timeout` if not already present (match web_fetch's client shape).

### 2. Handle 3xx responses explicitly

With `Policy::none()`, a 3xx is a normal response, not an error. Find where each a2a
request checks `response.status()` (call sites at `:285`, `:833`, `:986` and the
streaming/`agent.json` paths) and treat any 3xx status as an error result mentioning the
status + `Location` header, so a redirecting server is surfaced as
`Error::Tool { message: "A2A server returned redirect (3xx) to <location>" }` instead of
silently following or silently mis-parsing the redirect body. Do not attempt to follow.

### 3. Sanitize async-mode `task.id` before embedding

In the `use_async` branch (`src/tools/a2a.rs:358`), before building `poll_cmd`:
- If `task.id` does not match `^[A-Za-z0-9._-]+$`, fall back to **non-async** behavior for
  this call (return the plain task result with a warning line), or return
  `Error::Tool` — never embed an unvalidated id into the shell script.
- Keep the existing behavior for well-formed ids byte-identical.

### 4. Tests

In `src/tools/a2a.rs` tests (or a new `#[cfg(test)]` module):
- `build_client` has no redirect-following: assert `client` is built with
  `Policy::none()` — e.g. add a small test that inspects redirect behavior against a
  local `axum`/`tiny_http` stub returning 302 and asserts the tool surfaces the 3xx error
  and does NOT fetch the `Location` target (mirror whatever the file already uses for
  HTTP stubbing; if it has no stub infra, assert on the client config directly via a
  helper that exposes the policy — keep it minimal).
- Async-mode sanitization: a task whose `task.id` is `"; rm -rf /; echo "` must NOT
  produce a `poll_cmd` containing it; the call must fall back or error.
- Normal `task.id` like `t-123_abc.xyz` still produces the async script as today.

## Files NOT to touch

- `src/tools/url_guard.rs`, `src/tools/web_fetch.rs` — redirect policy already correct
  there; don't change web_fetch.
- `src/run_core.rs`, `src/kernel.rs`, `src/runtime.rs` — kernel/run-loop invariants.
- `.dev/flows/`, `.dev/scripts/`, `.flowcast/`.

## Acceptance

- `cargo build --workspace` green.
- `cargo test --workspace` green.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.
- Grep: `rg "Policy::none" src/tools/a2a.rs` returns ≥1 hit (client config).
- Grep: `rg "reject.*task.id|sanitize|\[A-Za-z0-9._-\]" src/tools/a2a.rs` returns ≥1 hit
  in the async branch.
- Headline test by name: `cargo test --manifest-path Cargo.toml a2a` — new tests green.

## Notes for the agent (traps)

- **Read `web_fetch.rs:37-75` first** — it is the canonical pattern: `Policy::none()` +
  explicit 3xx handling. Mirror its approach and error wording style.
- **The streaming path (`POST /message:stream`) and the `agent.json` card fetch use the
  same client** — verify all four a2a entry points (`a2a_call`, `a2a_card`,
  `a2a_task_check`, and the stream variant) go through `build_client()` or otherwise
  inherit the policy. If a path builds its own client, fix it too.
- **Do NOT follow redirects manually** (no loop over `Location`). Reject with the status.
- **Keep `task.id` validation minimal and strict** — the regex above is the contract.
  Do not URL-encode or shell-escape as a substitute for the whitelist; whitelist only.
- **Async-mode fallback**: the tool's contract is "returns TASK_ID for background
  polling" — falling back to synchronous for an invalid id is acceptable and safer than
  erroring the whole call. Prefer that.
- **cargo-fmt + clippy are enforced gates** — run both before finishing.
- **Journal**: write `.dev/journal/manual-<date>-goal378-a2a-redirect-and-taskid.md`.
