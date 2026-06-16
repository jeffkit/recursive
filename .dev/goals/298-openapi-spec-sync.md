# Goal 298 — Sync OpenAPI spec with actual response schemas

**Roadmap**: Post-Phase (API documentation correctness)

**Design principle check**:
- Implemented as: update `build_openapi_spec()` in `src/http/mod.rs` to
  accurately describe the actual response structures.
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

`build_openapi_spec()` in `src/http/mod.rs` is severely out of date.
Multiple recent changes (G293, G294, G295, G296, etc.) added fields to
response types but the OpenAPI spec was not updated:

### `SessionDetailResponse` (line ~926 in mod.rs)
Currently shows only 3 fields: `id`, `created_at`, `messages`.
**Actual fields** (from `src/http/handlers.rs`):
- `id: String`
- `created_at: String`
- `title: Option<String>`
- `messages: Vec<serde_json::Value>`
- `todos: Vec<...>`
- `status: String` (session status: idle, plan_pending_approval, etc.)
- `pending_plan: Option<String>`
- `goal: Option<String>`
- `first_prompt: Option<String>`
- `last_prompt: Option<String>`
- `prompt_tokens: u64` (added in G294)
- `completion_tokens: u64` (added in G294)

### `SessionInfo` (returned by `GET /sessions` inside `SessionList`)
The spec may not list: `title`, `message_count`.

### `/metrics` endpoint
Does not reflect new metrics added in G292:
`recursive_sessions_active` and `recursive_rate_limits_rejected_total`.

## Scope (do exactly this, no more)

### 1. `src/http/mod.rs` — update `SessionDetailResponse` schema

Replace the existing 3-field schema with the complete 12-field schema
matching `SessionDetailResponse` struct in `handlers.rs`:

```json
"SessionDetailResponse": {
    "type": "object",
    "properties": {
        "id": { "type": "string" },
        "created_at": { "type": "string" },
        "title": { "type": "string", "nullable": true },
        "messages": { "type": "array", "items": { "type": "object" } },
        "todos": { "type": "array", "items": { "type": "object" } },
        "status": { "type": "string", "description": "idle | plan_pending_approval" },
        "pending_plan": { "type": "string", "nullable": true },
        "goal": { "type": "string", "nullable": true },
        "first_prompt": { "type": "string", "nullable": true },
        "last_prompt": { "type": "string", "nullable": true },
        "prompt_tokens": { "type": "integer", "description": "Cumulative prompt tokens for this session" },
        "completion_tokens": { "type": "integer", "description": "Cumulative completion tokens for this session" }
    },
    "required": ["id", "created_at", "messages", "status", "todos", "prompt_tokens", "completion_tokens"]
}
```

### 2. Update `SessionInfo` schema

Ensure `SessionInfo` schema includes `message_count` and `title`:
```json
"SessionInfo": {
    "type": "object",
    "properties": {
        "id": { "type": "string" },
        "created_at": { "type": "string" },
        "message_count": { "type": "integer" },
        "title": { "type": "string", "nullable": true }
    },
    "required": ["id", "created_at", "message_count"]
}
```

### 3. Update the `/metrics` endpoint description

In the `GET /metrics` path description, add a note about the two new metrics
`recursive_sessions_active` and `recursive_rate_limits_rejected_total`.

### 4. Tests

Add a test that:
1. Calls `GET /openapi.json`
2. Parses the response JSON
3. Verifies `SessionDetailResponse.properties` contains at least `prompt_tokens`,
   `completion_tokens`, `status`, `todos`, `goal`

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `GET /openapi.json` returns a `SessionDetailResponse` schema with ≥10 properties
- `SessionInfo` schema includes `message_count` and `title`

## Notes for the agent

- Read `src/http/mod.rs` `build_openapi_spec()` and `SessionDetailResponse` struct
  in `src/http/handlers.rs` first.
- Read `src/http/mod.rs` `SessionInfo` struct definition.
- The OpenAPI spec is a big `serde_json::json!()` macro — be careful to
  keep the JSON structure valid when editing.
- **DO NOT modify** `src/agent.rs`, `src/runtime.rs`, `src/kernel.rs`.
