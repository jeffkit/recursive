# Goal 293 — GET /sessions: return total count alongside items

**Roadmap**: Post-Phase (API usability improvement)

**Design principle check**:
- Implemented as: wrap the `Vec<SessionInfo>` response in a `SessionList`
  envelope with `total` and `sessions` fields.
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

`GET /sessions` currently returns `Vec<SessionInfo>` (a bare JSON array).
Callers implementing paginated UIs cannot determine the total number of
sessions without fetching all pages — they have no way to compute "page X
of Y" or render a scrollbar, because the response carries no metadata about
the total.

The fix is a backward-incompatible schema change that introduces an envelope:

```json
{
  "total": 42,
  "sessions": [{ "id": "...", "created_at": "...", "message_count": 5 }, ...]
}
```

`total` is the count of **all** sessions (before offset/limit), so clients
can compute total pages.

## Scope (do exactly this, no more)

### 1. `src/http/handlers.rs` — new response type + update handler

Add:
```rust
#[derive(serde::Serialize)]
pub(super) struct SessionList {
    pub total: usize,
    pub sessions: Vec<SessionInfo>,
}
```

Change `list_sessions` return type from `Json<Vec<SessionInfo>>` to
`Json<SessionList>`:
```rust
let total = infos.len(); // count BEFORE offset/limit
let page: Vec<SessionInfo> = infos.into_iter().skip(offset).take(limit).collect();
Json(SessionList { total, sessions: page })
```

### 2. `src/http/mod.rs` — update OpenAPI spec if present

Find `build_openapi_spec()` and update the `GET /sessions` response schema
to reference the new envelope format.

### 3. Tests

- Existing tests that call `GET /sessions` and expect `Vec<SessionInfo>` must
  be updated to deserialize into `SessionList` and check `.sessions`.
- Add a test verifying `total` reflects the count before pagination:
  create 3 sessions, `GET /sessions?limit=2&offset=0`, check `total=3` and
  `sessions.len()=2`.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `GET /sessions` response is `{ "total": N, "sessions": [...] }`
- `total` reflects count before pagination, `sessions` is the paginated slice

## Notes for the agent

- Read `src/http/handlers.rs` `list_sessions`, `SessionInfo`, and the test
  helper that calls GET /sessions.
- Read `src/http/mod.rs` `build_openapi_spec()` for the sessions schema.
- The Python SDK (`sdk/python/recursive/client.py`) may call `/sessions`
  and parse the response as a list — update it if it exists.
- **DO NOT modify** `src/agent.rs`, `src/runtime.rs`, `src/kernel.rs`.
