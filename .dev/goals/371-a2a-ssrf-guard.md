# Goal 371 — Apply SSRF guard to a2a tools (no URL validation on outbound POSTs)

**Roadmap**: Tools / security — a2a tools can POST to arbitrary internal URLs

**Design principle check**:
- Implemented as: extract the existing `validate_url` + `is_private_ip` from
  `src/tools/web_fetch.rs` into a shared `crate::tools::url_guard` helper, call it at the
  top of the three a2a tool `execute` methods, plus regression tests.
- ❌ Does NOT touch the agent kernel, run loop, or any invariant. The a2a protocol
  behaviour for *legitimate* URLs is unchanged; only private/internal IPs are rejected
  before any socket opens.
- No new deps (`validate_url`/`is_private_ip` already exist in web_fetch.rs).

## Why (the SSRF hole, with evidence)

`src/tools/web_fetch.rs:51` already implements a SSRF guard:
```rust
fn validate_url(url: &str) -> Result<String> { ... }   // blocks 169.254/10.x/127.x/::1 etc.
fn is_private_ip(...) -> bool { ... }
```
This guards `WebFetch::execute`. **But `src/tools/a2a.rs` makes outbound HTTP to a
model-supplied `url` with NO such guard.** Three execute methods each take a `url`
argument and POST/poll to it:

- `src/tools/a2a.rs:228` — `A2aCallTool::execute` → POSTs to `{url}/message:send`
- `src/tools/a2a.rs:755` — `A2aCardTool::execute` → GETs `{url}/.well-known/agent-card.json`
- `src/tools/a2a.rs:869` — `A2aTaskCheckTool::execute` → polls `{url}/tasks/{id}`

The client builder at `src/tools/a2a.rs:151` (`build_client`) sets sane timeouts but **no
RedirectPolicy and no IP validation**. A model following a malicious instruction (or a
prompt-injected remote agent card) can drive the kernel to POST to
`http://169.254.169.254/latest/meta-data/...` (cloud IMDS), any RFC-1918 internal service,
or loopback — exfiltrating or touching internal endpoints.

**`a2a` is NOT feature-gated** — `src/tools/mod.rs:8` declares `pub mod a2a;`
unconditionally (compare `:16`/`:18` which DO gate `cloud-runtime` modules behind
`#[cfg(feature = ...)]`). So this surface ships in the default binary.

**Confirmed gap** (verified 2026-08-02): `grep -n 'validate_url\|is_private_ip' src/tools/a2a.rs` returns **0 hits** — a2a doesn't even import the guard. The existing test
`missing_prompt_returns_bad_tool_args_error` at `:1027` passes only because the *prompt*
arg is missing (fails the args check before any URL validation would run) — it is NOT an
SSRF test despite using `http://localhost`. This is the known NEW-TOOL-2 item that has
drifted across 3 review rounds.

## Scope (do exactly this, no more)

### 1. Extract the SSRF guard into a shared helper

Create `src/tools/url_guard.rs` (or `src/http/ssrf.rs` if cleaner — pick one; prefer
`tools/url_guard.rs` since both consumers are tools):
- Move `validate_url` and `is_private_ip` (and any helpers they use, e.g. the
  scheme-check + IP-parsing) from `src/tools/web_fetch.rs` into the new module.
- Make them `pub(crate)` (or `pub` if the existing call site needs `pub`; the goal is one
  definition, not a new public API — prefer `pub(crate)`).
- `web_fetch.rs` re-imports them: `use crate::tools::url_guard::{validate_url, is_private_ip};`
  and its behaviour is **byte-for-byte unchanged**. (Run `web_fetch`'s existing SSRF test
  to confirm.)

Register the module in `src/tools/mod.rs`: `pub(crate) mod url_guard;`

### 2. Wire the guard into all three a2a execute methods

At the top of each of the three `execute` methods (`:228`, `:755`, `:869`), right after
parsing the `url` argument, call:
```rust
let validated_url = crate::tools::url_guard::validate_url(&url)?;
```
This returns the normalised URL (or `Err(Error::BadToolArgs {...})` for private/internal).
Use `validated_url` (or just `url` if `validate_url` returns `()`) for the subsequent
`{url}/message:send` etc. construction. **Read `validate_url`'s actual signature in
`web_fetch.rs:51`** — it may return the trimmed/normalised URL string; mirror what
`WebFetch::execute` does with the return value.

Apply to ALL THREE methods — do not skip `A2aCardTool` or `A2aTaskCheckTool` (they GET/poll
the same attacker-controlled host).

### 3. Regression tests (`src/tools/a2a.rs` `#[cfg(test)]` module)

Add a test per execute method (or one parametrised-style test covering all three) that
asserts SSRF targets are rejected BEFORE any network call:
```rust
#[tokio::test]
async fn a2a_call_rejects_ssrf_targets() {
    let tool = A2aCallTool::new(/* whatever the constructor is */);
    for bad_url in ["http://127.0.0.1", "http://169.254.169.254",
                    "http://10.0.0.1", "http://[::1]"] {
        let args = serde_json::json!({"url": bad_url, "prompt": "hi"});
        let res = tool.execute(args).await;
        assert!(res.is_err(), "{bad_url} should be rejected");
        // assert the error is BadToolArgs (SSRF), not a network error:
        match res.unwrap_err() {
            Error::BadToolArgs { .. } => {},  // good
            other => panic!("expected BadToolArgs for {bad_url}, got {other:?}"),
        }
    }
}
```
Mirror for `A2aCardTool` and `A2aTaskCheckTool`. The point: assert the rejection happens at
the **args-validation** stage (no socket opened). Read the existing a2a test harness for
how it constructs the tool + what `execute`'s arg shape is.

A *positive* control is nice-to-have (a public URL is attempted and fails on network, not
on SSRF) but not required — the SSRF-rejection assertions are the gate.

### 4. Confirm web_fetch still passes

Run web_fetch's existing SSRF tests to confirm the extraction didn't regress it.

## Files NOT to touch

- `src/run_core.rs`, `src/kernel.rs`, `src/runtime.rs` — unrelated.
- The a2a protocol logic itself (message framing, agent-card parsing, task polling) — only
  add the URL guard at the entry point. Don't refactor the execute bodies.
- `web_fetch.rs`'s `validate_url` *logic* — move it verbatim, don't "improve" it. If you
  spot a bug in it, note it in the journal but don't fix it here (separate goal).
- Other tools (`shell.rs`, `edit.rs`) — out of scope.
- `.dev/flows/`.

## Acceptance

- `cargo test --workspace` green.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.
- Grep: `rg "validate_url|is_private_ip" src/tools/url_guard.rs` shows the helper now lives
  there; `rg "url_guard::validate_url" src/tools/a2a.rs` shows **≥3** call sites (one per
  execute method).
- The new SSRF-rejection tests pass; web_fetch's existing SSRF tests still pass.
- Reasoning check: on the OLD code, the new tests would make a real network call to
  127.0.0.1 (and likely hang or get connection-refused) — they would NOT return
  `BadToolArgs`. Post-fix they return `BadToolArgs` synchronously.

## Notes for the agent (traps)

- **`validate_url`'s return shape.** Read `web_fetch.rs:51` carefully — does it return
  `Result<String>` (the normalised URL) or `Result<()>`? Use its return the same way
  `WebFetch::execute` does. Don't invent a new signature.
- **The `{url}/path` join.** a2a builds URLs like `format!("{url}/message:send")`. Apply
  `validate_url` to the *base* `url` arg, not the joined path — the host is what matters
  for SSRF. Then build the path off the validated/normalised base.
- **All three methods, no skipping.** A common mistake is guarding `A2aCallTool` and
  forgetting `A2aCardTool`/`A2aTaskCheckTool`. The card endpoint does a GET, the task-check
  does a poll — both hit the same attacker host. All three.
- **Don't follow redirects either** — but that's a *separate* concern (Goal 372 covers the
  redirect bypass for web_fetch; a2a has the same hole but don't expand this goal's scope).
  Note it in the journal.
- **The "localhost" test at `:1027` is misleading** — it passes due to missing prompt, not
  SSRF. Your new tests must actually exercise the URL guard, not rely on coincidental
  arg-validation failures.
- **Extraction discipline.** When you move `validate_url`/`is_private_ip`, also move any
  `use` statements they depend on (e.g. `std::net::IpAddr`). The `web_fetch.rs` should
  end up importing from `url_guard` what it used to define inline — compile will tell you
  fast if you missed a helper.
