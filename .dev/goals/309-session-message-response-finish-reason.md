# Goal 309 — Add finish_reason and steps to SessionMessageResponse

**Roadmap**: Post-Phase (API informativeness)

**Design principle check**:
- Implemented as: adding `finish_reason: String` and `steps: usize` fields
  to `SessionMessageResponse` in `src/http/mod.rs` and populating them in
  `send_session_message` in `src/http/handlers.rs`
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

`POST /sessions/:id/messages` returns `SessionMessageResponse` with only
`role` and `content`. Clients have no way to know:
1. **Why the agent stopped** — cancelled, budget exceeded, goal achieved,
   or just finished normally (`NoMoreToolCalls`)
2. **How many tool calls were made** — useful for detecting stuck/looping
   agents and understanding run cost

Compare with `POST /run` (`RunResponse` from `src/http/mod.rs`), which includes:
```json
{
  "role": "assistant",
  "content": "...",
  "finish_reason": "NoMoreToolCalls",
  "steps": 5,
  "prompt_tokens": 1234,
  "completion_tokens": 456
}
```

`SessionMessageResponse` should expose at least `finish_reason` and `steps`
so clients can detect cancellation (`Cancelled`), budget (`BudgetExceeded`),
or goal achievement (`GoalAchieved`) and react accordingly.

## Scope (do exactly this, no more)

### 1. `src/http/mod.rs` — add fields to `SessionMessageResponse`

```rust
pub struct SessionMessageResponse {
    pub role: String,
    pub content: String,
    /// Why the agent turn ended (e.g. "NoMoreToolCalls", "BudgetExceeded",
    /// "Cancelled", "GoalAchieved").
    pub finish_reason: String,
    /// Number of tool-call steps taken during this turn.
    pub steps: usize,
}
```

### 2. `src/http/handlers.rs` — populate the new fields

In `send_session_message`, after the `outcome` is obtained (around line 994):

```rust
Ok(Json(SessionMessageResponse {
    role: "assistant".into(),
    content: last_assistant,
    finish_reason: outcome.finish_reason.to_string(),
    steps: outcome.steps,
}))
```

Note: `outcome.finish_reason` is a `crate::agent::FinishReason` which
implements `Display`. Call `.to_string()` on it.

Also update the `SessionMessageResponse` schema in `build_openapi_spec()`
in `src/http/mod.rs` to add `finish_reason` and `steps` fields.

### 3. Update the OpenAPI spec

In `build_openapi_spec()` (`src/http/mod.rs`), find `"SessionMessageResponse"`
and add the new fields:
```json
"SessionMessageResponse": {
    "type": "object",
    "properties": {
        "role": { "type": "string" },
        "content": { "type": "string" },
        "finish_reason": {
            "type": "string",
            "description": "Why the agent turn ended (NoMoreToolCalls, BudgetExceeded, Cancelled, GoalAchieved, etc.)"
        },
        "steps": {
            "type": "integer",
            "description": "Number of tool-call steps executed during this turn"
        }
    },
    "required": ["role", "content", "finish_reason", "steps"]
}
```

### 4. Tests

Add a test in `tests/http.rs` (or `src/http/handlers.rs` `#[cfg(test)]`) that:
1. Creates a session
2. Sends a message
3. Verifies the response includes `finish_reason` and `steps` fields
4. `finish_reason` is a non-empty string
5. `steps` is a non-negative integer

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `POST /sessions/:id/messages` response includes `finish_reason` and `steps`
- `finish_reason` is a non-empty string matching `FinishReason` variants

## Notes for the agent

- Read `src/http/mod.rs` around line 165 for `SessionMessageResponse`.
- Read `src/http/handlers.rs` around line 985–1000 for the response construction.
- `outcome.finish_reason` is `crate::agent::FinishReason` — read
  `src/agent.rs` to see its variants and `Display` impl.
- Read `src/http/mod.rs` `build_openapi_spec()` for the `SessionMessageResponse`
  schema (near end of file).
- The `RunResponse` struct is also in `src/http/mod.rs` — use it as a
  reference for how `finish_reason` and `steps` are added.
- **DO NOT modify** `src/agent.rs`, `src/runtime.rs`, `src/kernel.rs`,
  or any non-HTTP files.
