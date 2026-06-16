# Goal 313 — Migrate remaining plan/goal/interrupt handlers to ApiError

## Why

G304 migrated the major session CRUD handlers to `ApiError`, but five
handlers were left behind and still return the old `(StatusCode,
Json<serde_json::Value>)` pattern:

- `session_plan_confirm`  (line ~571)
- `session_plan_reject`   (line ~609)
- `session_set_goal`      (line ~644)
- `session_clear_goal`    (line ~679) — uses `axum::response::Response` variant
- `session_interrupt`     (line ~761)

These handlers produce `{"error": "..."}` JSON manually instead of using
the consistent `ApiError` envelope, making error handling inconsistent
across the API surface.

## Scope

**File to touch**: `src/http/handlers.rs`

Change the five handlers to return `Result<..., ApiError>` and use
`ApiError::not_found`, `ApiError::conflict`, `ApiError::ok_json` or the
appropriate `ApiError` constructor.

For `session_clear_goal` the current `axum::response::Response` variant
also sets a `Retry-After: 5` header on conflict — preserve this behaviour.
Use `.with_header(...)` or construct the response manually in the
conflict arm; the key is that the `404` and `409` arms must emit the
standard ApiError JSON envelope.

For `session_interrupt`, the current code already returns 200 when no token
is present (idempotent). Keep that behaviour.

## Tests

In `tests/http.rs`, add/update tests to assert the standard ApiError
JSON envelope for the error cases:

```
plan_confirm_returns_api_error_envelope_on_404
plan_reject_returns_api_error_envelope_on_404
session_set_goal_returns_api_error_envelope_on_404
session_interrupt_returns_api_error_envelope_on_404
```

Each test: send the request with a nonexistent session_id, assert
`status == 404`, deserialise the body, assert `body["error"]` is a
non-empty string (standard ApiError format).

Also assert that on 409 conflicts (plan_confirm without pending plan,
set_goal while busy) the body also contains `"error"` key.

## Acceptance criteria

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` no diff
- The five handlers all return `Result<..., ApiError>` (or use the
  `ApiError` type in their return/error path)
- Error JSON always has the `{"error": "..."}` shape from `ApiError`

## Notes for the agent

- Import path for `ApiError`: already imported at the top of `handlers.rs`
  as `build_openapi_spec, ApiError, AppState, ...`
- `ApiError::not_found("session not found")` — use consistently for 404
- `ApiError::conflict("...")` — use for 409
- For `session_clear_goal`'s CONFLICT arm that needs `Retry-After`: you
  can call `.into_response()` on a tuple of `(ApiError, HeaderMap)`, or
  build a manual `axum::http::Response` with the ApiError body. Simplest:
  return `Err(ApiError::conflict("session runtime is busy").with_retry_after(5))`
  if `ApiError` has a `with_retry_after` method — otherwise add one, or
  just set the header via the existing `into_response()` + `headers_mut()`
  pattern while still using ApiError for the 404 path.
- Do NOT change the 200 OK response bodies — keep the existing
  `{"status": "approved", ...}` etc shapes for those arms.
