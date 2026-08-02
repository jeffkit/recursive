# Goal 361 — Map `Error` variants to correct HTTP status codes (not blanket 500)

**Roadmap**: HTTP API correctness — error status-code mapping

**Design principle check**:
- Implemented as: a new `map_error_to_api(&Error) -> ApiError` helper + wiring it into the
  two run-entry handlers in `src/http/handlers.rs`.
- ❌ Does NOT touch the agent kernel, run loop, tools, or invariants. The runtime already
  returns typed `Error`; this goal only changes how the HTTP layer translates it.
- No new deps.

## Why (the misclassification, with evidence)

Both run-entry handlers collapse every `Error` variant into HTTP 500:

`src/http/handlers.rs:130-133` (`run_agent`):
```rust
let outcome = runtime.run(&body.goal).await.map_err(|e| {
    record_run_failed(&state.metrics);
    ApiError::internal(format!("agent run failed: {e}"))
})?;
```

`src/http/handlers.rs:949` (`send_session_message`): same `ApiError::internal(...)` pattern.

There is no per-variant mapping. Consequences:
- `Error::Cancelled` (user/client cancelled) → 500 (should be 499 Client Closed, or a 200
  with finish_reason=cancelled). A client retrying 500 on a cancel is wrong.
- `Error::PermissionDenied` / `PermissionDeniedLimit` → 500 (should be 403).
- `Error::RateLimited` (already carries `retry_after_ms`) → 500 (should be 429 with
  `Retry-After` header — the `ApiError::with_retry_after` helper at handlers.rs:440 EXISTS
  but is never used in this path).
- `Error::BadToolArgs` → 500 (should be 400).
- Genuine internal errors (LLM provider 5xx, IO failures) → 500 (correct, but lumped with
  the above).

A client cannot distinguish "your input was bad" from "we crashed" from "you got rate
limited, retry in N seconds" — all look like 500.

## Scope (do exactly this, no more)

### 1. Add a `map_run_error(&Error) -> ApiError` helper in `src/http/handlers.rs`

Pattern-match on `Error` variants to the right `ApiError` constructor. Read `src/error.rs`
first to get the FULL variant list and their fields, then map each. Suggested mapping
(verify variant names against the actual enum):

```rust
fn map_run_error(e: &crate::error::Error) -> ApiError {
    use crate::error::Error;
    match e {
        Error::Cancelled => ApiError:: /* 499 or a custom "client-cancelled" */,
        Error::PermissionDenied { .. } | Error::PermissionDeniedLimit { .. } =>
            ApiError::forbidden(e.to_string()),            // 403 — verify ApiError has this
        Error::RateLimited { retry_after_ms, .. } =>
            ApiError::with_retry_after(e.to_string(), *retry_after_ms),  // 429 — helper at :440
        Error::BadToolArgs { .. } =>
            ApiError::bad_request(e.to_string()),          // 400
        // Everything else is genuinely internal:
        _ => ApiError::internal(e.to_string()),            // 500
    }
}
```
**Read the existing `ApiError` type first** to see what constructors it offers
(`internal`, `with_retry_after`, `bad_request`, `forbidden`, `not_found`, etc.). Use the
real constructors — do not invent methods. If `ApiError` lacks a `forbidden`/`bad_request`,
either add them (minimal, one line each) or use the lowest-level constructor that takes a
status code.

For `Cancelled`: there's no standard 499 in axum/tower. The pragmatic choice is to return
a 200 with a JSON body `{ "finish_reason": "cancelled", ... }` OR a 499 if the helper
supports it. Read how the codebase currently handles client-disconnect (look for
`Connection`/hyper errors). If a clean 499 isn't available, map `Cancelled` to 503
(service-unavailable, retryable) — it's closer to correct than 500. Document the choice.

### 2. Wire it into both run-entry handlers

Replace the two `.map_err(|e| { record_run_failed(...); ApiError::internal(...) })?` sites
(`:130-133` and `:949`) with:
```rust
.map_err(|e| {
    record_run_failed(&state.metrics);
    map_run_error(&e)
})?;
```
Keep the `record_run_failed` call (metrics must still see the failure). Only the
`ApiError` construction moves into the helper.

### 3. Tests

Add tests in `tests/http.rs` (it already has 92 `#[tokio::test]` for HTTP). For each
mapped variant, drive the run endpoint with a runtime/LLM that returns that `Error` and
assert the response status code:
- `run_returns_403_on_permission_denied` — mock provider/tool that yields
  `Error::PermissionDenied`, assert 403.
- `run_returns_429_with_retry_after_on_rate_limited` — `Error::RateLimited { retry_after_ms: 1234 }`,
  assert 429 + `Retry-After: 1` header present.
- `run_returns_400_on_bad_tool_args` — assert 400.
- `run_returns_500_on_internal_llm_error` — `Error::Llm` from a 5xx provider response,
  assert 500 (regression guard: don't accidentally 4xx a real failure).

Read existing `tests/http.rs` for the established mock-provider + test-app harness pattern.
Mirror it. The mock must return the specific `Error` variant per test.

## Files NOT to touch

- `src/error.rs` — the `Error` enum is correct; this goal only maps it. (Exception: if a
  variant field like `retry_after_ms` doesn't exist where the map needs it, verify the
  real field name — don't add fields.)
- `src/run_core.rs`, `src/kernel.rs`, `src/runtime.rs`.
- The AG-UI SSE driver — that's goal 359's scope. This goal is the REST/JSON handlers.
- `.dev/flows/`.

## Acceptance

- `cargo test --workspace` green, including the 4 new status-code tests.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.
- Grep: `rg "ApiError::internal" src/http/handlers.rs` shows the `run_agent` / 
  `send_session_message` paths no longer use blanket `internal` — they call `map_run_error`.

## Notes for the agent (traps)

- **Read `ApiError` first.** It's defined in `src/http/mod.rs` or `src/http/error.rs`. Use
  its REAL constructors. Don't assume `forbidden()`/`bad_request()` exist; if they don't,
  add minimal ones or use the status-code-level constructor.
- **`Error::RateLimited` field name.** The review cited `retry_after_ms`; verify the actual
  field name in `src/error.rs`. The `with_retry_after` helper at `:440` takes a duration in
  ms or seconds — check its signature and convert appropriately.
- **`Error::Cancelled` HTTP semantics.** 499 isn't standard. If the framework doesn't
  support it, prefer 503 (retryable service unavailable) over 500 — a client retrying a
  cancel as 500 is the current bug; 503 at least signals "transient, ok to retry". Document
  the choice in the journal. Do NOT map cancel to 200-with-body unless the existing API
  convention already does that for other finish-reasons.
- **Don't map every variant.** Unknown/genuinely-internal variants stay 500 via the
  catch-all `_ =>`. Don't over-engineer per-variant codes for `Error::Io`/`Error::Json`/
  `Error::Mcp` — those are 500 (server-side, not client's fault).
- **`record_run_failed` stays.** The metrics recording is orthogonal to the status code;
  keep it for ALL error variants, even 4xx ones (a 403 still "failed" from a metrics view).
- **Test the headers, not just status.** The 429 test MUST assert the `Retry-After` header
  value, not just 429 — that's the regression-prone part.
