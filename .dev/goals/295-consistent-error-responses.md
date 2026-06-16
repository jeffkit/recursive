# Goal 295 — Standardize HTTP error responses to JSON `{"error": "..."}` body

**Roadmap**: Post-Phase (API consistency)

**Design principle check**:
- Implemented as: create an `ApiError` newtype that implements `IntoResponse`
  and wraps `(StatusCode, Json<ErrorBody>)`; replace bare `StatusCode` returns
  in handlers with `ApiError`.
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

`src/http/handlers.rs` has two conflicting error response patterns:

1. **Bare status codes** (20+ sites): `return Err(StatusCode::NOT_FOUND)`.
   These produce HTTP 4xx/5xx with **no body** — clients only get a numeric code.

2. **JSON error bodies** (several sites): 
   `return Err((StatusCode::NOT_FOUND, Json(json!({"error": "session not found"}))))`.
   These are parseable.

An API client cannot write a single error handler that works for both patterns.
Some clients will see `{}` for bare-status-code errors, others will crash
trying to deserialize an empty body as JSON.

## Scope (do exactly this, no more)

### 1. `src/http/mod.rs` — introduce `ApiError` type

```rust
/// A standardized JSON error response for all API endpoints.
#[derive(serde::Serialize)]
struct ErrorBody {
    error: String,
}

pub struct ApiError(StatusCode, String);

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self(status, message.into())
    }
    // Convenience constructors:
    pub fn not_found(msg: impl Into<String>) -> Self { Self::new(StatusCode::NOT_FOUND, msg) }
    pub fn conflict(msg: impl Into<String>) -> Self { Self::new(StatusCode::CONFLICT, msg) }
    pub fn internal(msg: impl Into<String>) -> Self { Self::new(StatusCode::INTERNAL_SERVER_ERROR, msg) }
    pub fn bad_request(msg: impl Into<String>) -> Self { Self::new(StatusCode::BAD_REQUEST, msg) }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (self.0, Json(ErrorBody { error: self.1 })).into_response()
    }
}
```

### 2. `src/http/handlers.rs` — replace bare `StatusCode` errors

Find all handler functions that return `Result<_, StatusCode>` and return
`Err(StatusCode::NOT_FOUND)` etc. Replace with `Err(ApiError::not_found(...))`
etc. for the most common cases:

- `StatusCode::NOT_FOUND` → `ApiError::not_found("session not found")` (or appropriate message)
- `StatusCode::CONFLICT` → `ApiError::conflict("session is busy")`
- `StatusCode::INTERNAL_SERVER_ERROR` → `ApiError::internal("internal error")`
- `StatusCode::BAD_REQUEST` → `ApiError::bad_request("...")`

Update the handler return types from `Result<Json<T>, StatusCode>` to
`Result<Json<T>, ApiError>`.

Do NOT change the existing `(StatusCode, Json(json!(...)))` error sites —
they are already correct. Only replace the bare `StatusCode` ones.

**Limit scope**: only replace handlers in `handlers.rs`. Don't touch auth,
rate-limit, or middleware error paths (they are intentionally thin).

### 3. Tests

Existing tests that check `status.is_client_error()` or similar should still
pass. Add one test verifying that a 404 response has a JSON body with `"error"`
key (e.g. get a nonexistent session and check the body is parseable JSON).

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `GET /sessions/nonexistent` returns `{"error": "session not found"}` body
- No handler in `handlers.rs` returns bare `Err(StatusCode::*)` without a body

## Notes for the agent

- Read `src/http/handlers.rs` and `src/http/mod.rs` first.
- The `ApiError` type lives in `mod.rs` (or a new `src/http/error.rs`) and is
  `pub(super)` for use within the `http` module.
- You may need to update the handler return type annotations (e.g.
  `Result<Json<T>, ApiError>` instead of `Result<Json<T>, StatusCode>`).
- Be careful: some handlers return `Result<StatusCode, StatusCode>` for
  operations with no body (DELETE). Those can become `Result<StatusCode, ApiError>`.
- **DO NOT modify** `src/agent.rs`, `src/runtime.rs`, `src/kernel.rs`.
