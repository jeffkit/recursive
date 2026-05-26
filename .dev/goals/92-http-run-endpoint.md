# Goal 92 — HTTP API: POST /run — one-shot execution

**Roadmap**: Phase 12.2 — HTTP API (part 2/6)

**Design principle check**:
- Implemented as: new route in `src/http.rs` + handler that uses existing `Agent`
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- Uses Agent as a black box — build, run, return result

## Why

The `/tools` endpoint proves the server works. The next essential capability
is executing the agent via HTTP: a client POSTs a goal, the server runs
the agent to completion, and returns the final response. This enables
programmatic access from SDKs, scripts, and the future TUI.

## Scope (do exactly this, no more)

### 1. `src/http.rs` — add POST /run endpoint

Request body:
```json
{
  "goal": "string (required)",
  "max_steps": 50,          // optional, default from config
  "system_prompt": null     // optional override
}
```

Response (200):
```json
{
  "status": "completed",
  "finish_reason": "NoMoreToolCalls",
  "messages": [...],        // final transcript messages
  "usage": {
    "total_steps": 5,
    "total_tokens": 1234
  }
}
```

Response (500 on agent error):
```json
{
  "status": "error",
  "error": "description"
}
```

### 2. Expand `AppState`

```rust
pub struct AppState {
    pub tools: Vec<ToolInfo>,
    pub config: Config,          // NEW — needed to build agent
}
```

### 3. Handler implementation

```rust
async fn run_agent(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RunRequest>,
) -> impl IntoResponse { ... }
```

The handler:
1. Clones config from state, applies overrides (max_steps, system_prompt)
2. Builds an Agent with the config
3. Calls `agent.run().await`
4. Returns structured JSON with outcome

### 4. `src/main.rs` — update Serve handler

Pass `config` into AppState when constructing the HTTP server.

### 5. Tests

In `tests/http.rs`:
- Test: POST /run with mock provider returns 200 + valid response shape
- Test: POST /run with missing "goal" field returns 400
- Test: POST /run response has expected fields (status, finish_reason, usage)

Use the `test-utils` feature + `MockProvider` to avoid real LLM calls.

## Acceptance

- `cargo test` green (all existing + new tests)
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- POST /run with a mock provider works end-to-end in tests

## Notes for the agent

- Read `src/http.rs` for the existing scaffold (AppState, build_router, ToolInfo).
- Read `src/agent.rs` for `AgentBuilder` and `Agent::run()` → returns `AgentOutcome`.
- Read `src/config.rs` for `Config` struct fields.
- Read `src/llm/mock.rs` for `MockProvider` usage in tests.
- `AgentOutcome` has fields: `finish_reason: FinishReason`, `transcript: Vec<Message>`,
  `usage: TokenUsage`.
- For tests, construct a `Config` with `MockProvider` and a minimal tool set.
- `Config` needs to be `Clone` — check if it already is; if not, derive it.
- **DO NOT modify `src/agent.rs` or `src/tools/`.**
- **DO NOT add real LLM calls in tests.**
- **Keep the handler simple — no streaming, no sessions yet (those are g93+).**
