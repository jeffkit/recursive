# Goal 304 — Complete migration of remaining handlers to ApiError

**Roadmap**: Post-Phase (G295 follow-up)

**Design principle check**:
- Implemented as: updating 3 handler return types in `src/http/handlers.rs`
  to use `ApiError` (introduced in G295)
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

Goal-295 introduced `ApiError` to standardize HTTP error responses across
the HTTP API. However, the agent that ran G295 only converted some handlers.
Three handlers still use the old `(StatusCode, Json<ErrorResponse>)` pattern:

1. `run_agent` (line ~65): `Result<Json<RunResponse>, (StatusCode, Json<ErrorResponse>)>`
2. `create_session` (line ~208): `Result<(StatusCode, Json<CreateSessionResponse>), (StatusCode, Json<ErrorResponse>)>`
3. `send_session_message` (line ~833): `Result<Json<SessionMessageResponse>, (StatusCode, Json<ErrorResponse>)>`

These return raw `(StatusCode, Json<ErrorResponse>)` tuples, which produce
inconsistent error JSON (the old `{status, error}` shape from `ErrorResponse`
vs. the new `{error}` shape from `ApiError`). Clients see different error
shapes depending on which endpoint fails.

## Scope (do exactly this, no more)

### 1. `src/http/handlers.rs` — convert 3 remaining handlers

**`run_agent`**: Change return type to `Result<Json<RunResponse>, ApiError>`.
Replace each error return with `ApiError::bad_request(...)`,
`ApiError::service_unavailable(...)`, etc. as appropriate.

**`create_session`**: Change return type to
`Result<(StatusCode, Json<CreateSessionResponse>), ApiError>`.

**`send_session_message`**: Change return type to
`Result<Json<SessionMessageResponse>, ApiError>`.

For each error site in these functions, replace the old pattern:
```rust
Err((
    StatusCode::BAD_REQUEST,
    Json(ErrorResponse { status: "error".into(), error: "msg".into() }),
))
```
With:
```rust
Err(ApiError::bad_request("msg"))
```

Use the appropriate `ApiError` constructor:
- `ApiError::bad_request(msg)` → 400
- `ApiError::not_found(msg)` → 404
- `ApiError::conflict(msg)` → 409
- `ApiError::service_unavailable(msg)` → 503
- `ApiError::internal(msg)` → 500
- `ApiError::new(StatusCode::X, msg)` → for any other status code

### 2. Remove unused `ErrorResponse` import (if no longer used)

After converting all 3 handlers, check if `ErrorResponse` is still imported
in `src/http/handlers.rs`. If not, remove the import to keep the file clean.
Also check if `ErrorResponse` struct in `src/http/mod.rs` is still needed
(it may still be used for backward compatibility or by tests).

### 3. Tests

Add tests in `tests/http.rs` (or the existing test module) that call
`POST /run`, `POST /sessions`, and `POST /sessions/:id/messages` with invalid
inputs and verify they return `{"error": "..."}` JSON (not `{"status":...,"error":...}`).

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- No `(StatusCode, Json<ErrorResponse>)` tuple error returns remain in
  `src/http/handlers.rs` outside of `#[cfg(test)]`
- `grep -n "Json<ErrorResponse>" src/http/handlers.rs` shows only import lines

## Notes for the agent

- Read `src/http/mod.rs` to understand `ApiError` (its constructors and the
  `IntoResponse` implementation that produces `{"error": "..."}` JSON).
- Read `src/http/handlers.rs` lines 62–200 for `run_agent`, lines 200–290
  for `create_session`, and lines 830–1005 for `send_session_message`.
- The three handlers may have multiple error return sites — convert each one.
- `ErrorResponse` might still be needed in `mod.rs` for backward compatibility
  even if no longer used in handlers (don't delete it from mod.rs).
- **DO NOT modify** `src/agent.rs`, `src/runtime.rs`, `src/kernel.rs`,
  or any non-HTTP files.
