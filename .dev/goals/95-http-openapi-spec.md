# Goal 95 — HTTP API: OpenAPI Spec Generation

**Roadmap**: Phase 12.5 — HTTP API (part 5/6)

**Design principle check**:
- Implemented as: static JSON/YAML spec + GET /openapi.json endpoint in `src/http.rs`
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- Read-only introspection endpoint, no behavioral change

## Why

An OpenAPI spec lets external tools (Swagger UI, code generators, SDK
builders) understand our API without reading Rust source. It's also the
foundation for the Python SDK (g96).

## Scope (do exactly this, no more)

### 1. `src/http.rs` — add GET /openapi.json endpoint

Hand-craft an OpenAPI 3.0 spec as a `serde_json::Value` constant (or
lazy_static / once_cell). No need for a proc-macro or code-gen crate.

The spec should document these endpoints:
- GET /health
- GET /tools
- POST /run
- POST /sessions
- GET /sessions
- GET /sessions/:id
- DELETE /sessions/:id
- POST /sessions/:id/messages
- GET /sessions/:id/events

For each endpoint, include:
- Path and method
- Request body schema (where applicable)
- Response schema (200/201/204/400/404/500)
- Brief description

### 2. Route registration

```rust
.route("/openapi.json", get(openapi_spec))
```

Handler:
```rust
async fn openapi_spec() -> Json<serde_json::Value> {
    Json(build_openapi_spec())
}

fn build_openapi_spec() -> serde_json::Value {
    serde_json::json!({
        "openapi": "3.0.3",
        "info": {
            "title": "Recursive Agent API",
            "version": "0.4.0",
            "description": "HTTP API for the Recursive coding agent"
        },
        "paths": { ... }
    })
}
```

### 3. Tests

- Test: GET /openapi.json returns 200 with valid JSON
- Test: Response contains "openapi" field with value "3.0.3"
- Test: Response contains paths for all registered endpoints

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- GET /openapi.json returns a valid OpenAPI 3.0 document

## Notes for the agent

- Read `src/http.rs` for the current router and all endpoint schemas.
- No new dependencies needed — just `serde_json::json!()` macro.
- The spec doesn't need to be 100% complete schema-wise — basic path +
  method + description + status codes is sufficient for this goal.
- **DO NOT add utoipa or other OpenAPI crates — hand-craft the JSON.**
- **DO NOT modify any endpoint behavior.**
- **Keep it under 150 lines for the spec builder function.**
