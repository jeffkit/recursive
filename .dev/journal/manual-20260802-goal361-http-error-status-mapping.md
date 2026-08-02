# Manual edit: goal-361 — Map `Error` variants to correct HTTP status codes (not blanket 500)

**Date**: 2026-08-02
**Goal**: Stop collapsing every run failure into HTTP 500. The two run-entry
handlers (`run_agent` at handlers.rs:130-133 and `send_session_message` at
handlers.rs:949) both mapped any `Error` to `ApiError::internal(...)`, so a
client could not distinguish bad input (should be 400) from rate limiting
(should be 429 + Retry-After) from a genuine server crash (500). This goal is
HTTP-layer only — `src/error.rs`, `src/run_core.rs`, `src/kernel.rs`,
`src/runtime.rs` untouched; no new deps.

## Files touched

- `src/http/handlers.rs`
  - Added `fn map_run_error(e: &crate::error::Error) -> ApiError` (single
    place that translates a typed runtime error to the right status code).
  - Wired it into `run_agent` (`runtime.run(...).map_err`) and
    `send_session_message` (`run_result.map_err`), keeping
    `record_run_failed(&state.metrics)` in both paths (metrics recording is
    orthogonal to the status code and must see ALL failures, even 4xx).
    Note: the `send_session_message` path previously did NOT call
    `record_run_failed`; per the goal snippet it now does (a 403/429/500 from
    that endpoint is still a "failed" run from a metrics view).
- `src/http/mod.rs` — added `ApiError::forbidden(message)` (403) constructor,
  one line, mirroring the existing `bad_request`/`not_found` style.
- `tests/http.rs` — 4 new tests (see below).

## Mapping (documented in the helper's doc comment)

| `Error` variant | HTTP | Constructor |
|---|---|---|
| `Cancelled` | 503 | `ApiError::new(SERVICE_UNAVAILABLE, ...)` |
| `PermissionDenied` / `PermissionDeniedLimit` | 403 | `ApiError::forbidden` |
| `RateLimited { retry_after_ms }` | 429 + `Retry-After` | `ApiError::new(TOO_MANY_REQUESTS, ...).with_retry_after(ms/1000)` |
| `BadToolArgs` | 400 | `ApiError::bad_request` |
| everything else (`Llm`, `Io`, `Json`, `Mcp`, `Timeout`, `Tool`, `Internal`, …) | 500 | `ApiError::internal` (catch-all `_`) |

- **`Cancelled` → 503, not 499/200**: 499 is a non-standard nginx code and
  axum/tower has no constant for it; the goal notes explicitly prefer 503
  (transient, retryable) over 500 for a cancel, and explicitly say NOT to map
  cancel to 200-with-body unless the API already does that for other
  finish-reasons (it doesn't). Documented in the helper doc comment. Note the
  runtime already converts *mid-stream* LLM cancels into
  `FinishReason::Cancelled` outcomes (run_core.rs:1317), so an
  `Err(Error::Cancelled)` reaching the HTTP layer is the exceptional path.
- **429 conversion**: `Error::RateLimited.retry_after_ms` is u64 ms;
  `ApiError::with_retry_after` takes whole **seconds** (u32) — verified against
  the real signature in `src/http/mod.rs` (it is a builder on `self`, NOT
  `new`-style; the goal's sketch `ApiError::with_retry_after(msg, ms)` would
  not compile). Converted with floor division `(*retry_after_ms / 1000) as u32`
  so 1234ms → `Retry-After: 1` (sub-second waits become `Retry-After: 0`).
- Deliberately did NOT map `Io`/`Json`/`Mcp`/`Llm`/`Timeout`/`Tool` — those are
  server-side failures, 500 is correct. Catch-all `_` keeps them 500.
- Remaining `ApiError::internal` uses in handlers.rs (runtime-build failures at
  ~:167/:288/:598) are genuinely internal and out of scope; the run-failure
  paths now call `map_run_error`.

## Tests added

All in `tests/http.rs`, using the established `sample_state_with_provider` +
`build_router` + `MockProvider::with_errors(vec![...])` harness (error queue
popped on the first `complete()`; with `retry_max: 0` no outer retry loop, the
error propagates immediately — same pattern as `runtime.rs`'s
`llm_retry_emits_event`). The mock returns the specific `Error` variant per
test:

- `run_returns_403_on_permission_denied` — `Error::PermissionDenied { name,
  reason: DecisionReason::Mode(PermissionMode::DontAsk) }`, asserts 403 + body
  contains "permission denied".
- `run_returns_429_with_retry_after_on_rate_limited` —
  `Error::RateLimited { retry_after_ms: 1234 }`, asserts 429 AND
  `Retry-After: 1` header (the regression-prone part — floor conversion).
- `run_returns_400_on_bad_tool_args` — `Error::BadToolArgs { name, message }`,
  asserts 400 + body contains "bad tool arguments".
- `run_returns_500_on_internal_llm_error` — `Error::Llm` (upstream 5xx),
  asserts 500 (regression guard: don't accidentally 4xx a real failure).

## Quality gates (run by hand, all green)

- `cargo fmt --all` — clean.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — clean.
- `cargo test --workspace` — clean (0 failures; includes 97 http tests, 4 new).
- `rg "ApiError::internal" src/http/handlers.rs` — run-failure paths no longer
  use blanket `internal`; only the catch-all `_ =>` inside `map_run_error` and
  the runtime-build (out-of-scope) sites remain.

## Notes / traps

- **`with_retry_after` is a builder, not a constructor.** It takes `self` +
  whole seconds (u32). The goal's sketch would not compile; used
  `ApiError::new(StatusCode::TOO_MANY_REQUESTS, ...).with_retry_after(secs)`.
- **`forbidden` did not exist** on `ApiError`; added it (one line) per the
  goal's "add them (minimal, one line each)" option rather than scattering
  `ApiError::new(StatusCode::FORBIDDEN, ...)`.
- **`record_run_failed` added to `send_session_message`** — the goal's snippet
  includes it at both sites; previously only `run_agent` recorded failures.
- The AG-UI SSE driver (handlers.rs:1843 `code: None` comment) is Goal 359's
  territory and was NOT touched; the REST/JSON handlers are the only wiring
  changed.
