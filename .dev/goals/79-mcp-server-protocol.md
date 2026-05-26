# Goal 79 — MCP Server: JSON-RPC Protocol Layer

**Roadmap**: Phase 6.3 part 1/3 — Protocol types and dispatch

**Design principle check**:
- Implemented as: new file `src/mcp_server.rs` with only types and a
  dispatch function. No IO, no stdin/stdout, no agent loop changes.

## Why

MCP Server Mode (g68) failed twice because the agent tried to do
everything at once in a 2000-line file. This goal handles ONLY the
protocol layer: parsing JSON-RPC requests, routing to handlers, and
formatting responses. Pure functions, easily testable.

## Scope (do exactly this, no more)

### 1. `src/mcp_server.rs` — new file, protocol types only

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A JSON-RPC 2.0 request.
#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<Value>,  // None for notifications
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// A JSON-RPC 2.0 response.
#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
}

impl JsonRpcResponse {
    pub fn success(id: Value, result: Value) -> Self { ... }
    pub fn error(id: Value, code: i32, message: String) -> Self { ... }
    pub fn parse_error() -> Self { ... }       // code -32700
    pub fn method_not_found(id: Value) -> Self { ... }  // code -32601
}
```

### 2. `src/mcp_server.rs` — dispatch function

```rust
use crate::tools::ToolRegistry;
use crate::llm::ToolSpec;

/// Dispatch a parsed JSON-RPC request and return a response.
/// Returns None for notifications (no response needed).
pub async fn dispatch_request(
    request: &JsonRpcRequest,
    tool_specs: &[ToolSpec],
    tools: &ToolRegistry,
) -> Option<JsonRpcResponse> {
    match request.method.as_str() {
        "initialize" => Some(handle_initialize(request)),
        "notifications/initialized" => None,  // notification, no response
        "tools/list" => Some(handle_tools_list(request, tool_specs)),
        "tools/call" => Some(handle_tools_call(request, tools).await),
        _ => Some(JsonRpcResponse::method_not_found(request.id.clone().unwrap_or(Value::Null))),
    }
}
```

### 3. Handler functions (private)

- `handle_initialize` → return capabilities + server info
- `handle_tools_list` → map ToolSpec vec to MCP tool list format
- `handle_tools_call` → extract tool name + args, call tool, return content

### 4. `src/lib.rs` — add module

```rust
pub mod mcp_server;
```

### 5. Tests (at least 8)

- Test: parse valid JSON-RPC request
- Test: parse notification (no id)
- Test: JsonRpcResponse::success serializes correctly
- Test: JsonRpcResponse::error serializes correctly
- Test: dispatch "initialize" returns capabilities
- Test: dispatch "tools/list" returns tool specs
- Test: dispatch "tools/call" executes tool and returns content
- Test: dispatch unknown method returns -32601
- Test: dispatch notification returns None

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets -- -D warnings` clean
- New module compiles and all tests pass
- NO stdin/stdout IO in this file — that's goal 80

## Notes for the agent

- This is ONLY the protocol layer. Do NOT read from stdin or write to stdout.
- Read `src/mcp.rs` for how the CLIENT formats requests — the server does
  the mirror.
- Read `src/tools/mod.rs` for `ToolRegistry` and how to call `execute`.
- The `tools/list` response format:
  ```json
  {"tools": [{"name": "...", "description": "...", "inputSchema": {...}}]}
  ```
- The `tools/call` response format:
  ```json
  {"content": [{"type": "text", "text": "result here"}]}
  ```
- For `tools/call` error: use `{"isError": true, "content": [{"type":"text","text":"error msg"}]}`
- Keep it simple: ~200 LOC max for this goal.
