# Goal 176 — `a2a_call` Tool: Invoke a Remote A2A Agent

**Roadmap**: Phase 18 — Advanced Agent Patterns (cross-agent interop)  
**Design principle check**:
- Implemented as: a new tool `src/tools/a2a.rs`; no agent loop changes.
- ❌ Does NOT branch inside `agent.rs::Agent::run`.
- ✅ Additive: purely new capability.

## Why

A2A (Agent2Agent) v1.0 is an open standard (Linux Foundation, formerly Google)
for inter-agent communication over HTTP+JSON. It allows agents built by different
vendors/frameworks to delegate tasks to each other. Giving Recursive's agent a
built-in `a2a_call` tool lets it reach out to any A2A-compliant remote agent
(e.g. Google, AWS, Azure, AgentStudio agents) without writing custom HTTP code.

## Protocol overview

A2A uses:
- **Agent Card**: JSON manifest at `/.well-known/agent-card.json` — describes
  capabilities, endpoint URLs, auth schemes.
- **HTTP REST binding**: Primary binding:
  - `POST /message:send` → sends a message; returns `{ task }` or `{ message }`
  - `GET /tasks/{id}` → polls task status
  - `POST /message:stream` → streaming SSE (out of scope for MVP)
- **Request body** (`POST /message:send`):
  ```json
  {
    "message": {
      "role": "ROLE_USER",
      "parts": [{"text": "…"}],
      "messageId": "<uuid>"
    }
  }
  ```
- **Response**: Either a `Task` (async) or a direct `Message`.
  - `Task.status.state` values: `TASK_STATE_SUBMITTED`, `TASK_STATE_WORKING`, `TASK_STATE_COMPLETED`, `TASK_STATE_FAILED`, `TASK_STATE_CANCELED`, `TASK_STATE_REJECTED`
  - `Task.artifacts[].parts[].text` contains the final output.
- **Content-Type**: `application/a2a+json`

## What this goal does

### 1. New tool: `src/tools/a2a.rs`

**Tool name**: `a2a_call`

**Parameters**:
```json
{
  "type": "object",
  "properties": {
    "url": {
      "type": "string",
      "description": "Base URL of the A2A server (e.g. 'https://agent.example.com'). The tool will POST to {url}/message:send and poll {url}/tasks/{id}."
    },
    "prompt": {
      "type": "string",
      "description": "The text message to send to the remote agent."
    },
    "authorization": {
      "type": "string",
      "description": "Optional Authorization header value (e.g. 'Bearer <token>'). Omit for public agents."
    },
    "timeout_secs": {
      "type": "integer",
      "description": "Maximum seconds to wait for a completed task (default: 60, max: 300). During this time, the tool polls the task every 2 seconds.",
      "default": 60
    }
  },
  "required": ["url", "prompt"]
}
```

**Side effect**: `ToolSideEffect::External`

**Execution flow**:

1. Generate a random `messageId` (UUID v4).
2. `POST {url}/message:send` with:
   - `Content-Type: application/a2a+json`
   - Optional `Authorization` header
   - Body: `{"message": {"role": "ROLE_USER", "parts": [{"text": prompt}], "messageId": uuid}}`
3. Parse response:
   - If `response.message` is present → extract text parts → return directly.
   - If `response.task` is present → enter polling loop.
4. Polling loop (while `task.status.state` is `SUBMITTED` or `WORKING`):
   - Sleep 2 seconds
   - `GET {url}/tasks/{task.id}` with same auth header
   - Update `task` from response
   - Check timeout
5. When `state == COMPLETED`:
   - Collect all text from `task.artifacts[].parts[].text`
   - Return combined text
6. If `state` is `FAILED` / `CANCELED` / `REJECTED` / timeout:
   - Return `ERROR: task {id} ended with state {state}`

**Error handling**:
- HTTP error → `ERROR: HTTP {status}: {body}`
- JSON parse error → `ERROR: invalid A2A response: {msg}`
- Timeout → `ERROR: task {id} timed out after {n}s`
- Network error → `ERROR: network error: {msg}`

### 2. Register in `src/tools/mod.rs`

Add `pub mod a2a;` and register `A2aCallTool` in `build_standard_tools()` and `build_tools()`.

### 3. Tests

`src/tools/a2a.rs` — unit tests using `mockito` or a hand-rolled mock server:
- `a2a_call_immediate_message_response`: server returns `{message}` directly → tool returns text
- `a2a_call_completed_task_response`: server returns `{task: {status: COMPLETED, artifacts: …}}` → tool returns artifact text
- `a2a_call_polling_task_to_completion`: first call returns `WORKING` task, second poll returns `COMPLETED` → tool polls and returns text
- `a2a_call_task_failed`: task ends with `FAILED` → tool returns error string
- `a2a_call_timeout`: task stays `WORKING` beyond timeout → tool returns timeout error
- `a2a_call_missing_prompt`: no `prompt` arg → `BadToolArgs` error

> Note: Use `mockito` for mock HTTP server. If `mockito` is not in `Cargo.toml`,
> use `tokio::task::spawn` + `axum` (already in dev-deps) to spin up a test server.
> Check Cargo.toml first.

### 4. Re-export

`src/lib.rs` does not need to re-export `A2aCallTool` (internal tool, not public API).

## Files to change

| File | Change |
|------|--------|
| `src/tools/a2a.rs` (new) | `A2aCallTool`, A2A data structures, HTTP logic, tests |
| `src/tools/mod.rs` | `pub mod a2a;` + register `A2aCallTool` |

## Out of scope

- Streaming (`POST /message:stream`) — separate goal
- Agent Card discovery / capability validation
- Push notifications / webhooks
- Per-call MCP tool injection to the remote agent
- Auth token refresh / OAuth flow

## Acceptance

1. `cargo test --workspace` green (excluding `mcp_e2e` which requires live server)
2. `cargo clippy --all-targets --all-features -- -D warnings` clean
3. `cargo fmt --all` clean
4. All 6 unit tests pass
5. `a2a_call` appears in `build_standard_tools()` → visible to agents

## Reference

- A2A v1.0 spec: https://a2a-protocol.org/v1.0.0/specification/
- Key section: 6.1 Basic Task Execution, 5.3 Method Mapping Reference
