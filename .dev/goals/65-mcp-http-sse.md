# Goal 65 — MCP HTTP+SSE Transport (Streamable HTTP)

**Roadmap**: Phase 6.1 — MCP HTTP+SSE transport

**Design principle check**:
- Implemented as: new transport variant in `src/mcp.rs`. Does not modify
  the agent loop.
- Does NOT modify `agent.rs`.

## Why

The current MCP implementation only supports stdio transport (spawn a
subprocess, communicate via stdin/stdout JSON-RPC). The MCP spec also
defines HTTP+SSE transport for remote MCP servers: POST JSON-RPC to an
HTTP endpoint, receive responses (and notifications) via Server-Sent
Events on the same or a separate endpoint.

This unlocks connecting to remote MCP servers (cloud-hosted tools,
shared infrastructure) without running local processes.

## Scope (do exactly this, no more)

### 1. `src/mcp.rs` — add `HttpSseTransport` alongside `StdioTransport`

Create a new transport that:
- Sends JSON-RPC requests via HTTP POST to a configured endpoint URL
- Receives responses via SSE (Server-Sent Events) on a streaming endpoint
- Supports the MCP Streamable HTTP spec:
  - POST to endpoint with `Content-Type: application/json`
  - Response is `text/event-stream` with JSON-RPC response events
  - Each SSE event has `data:` lines containing a JSON-RPC response

Structure:
```rust
pub struct HttpSseTransport {
    /// Base URL for the MCP server (e.g. http://localhost:8080)
    endpoint: String,
    /// HTTP client
    client: reqwest::Client,
}
```

Implement the same interface as StdioTransport:
- `send_request(&self, method: &str, params: Value) -> Result<Value>`
- `initialize(&mut self) -> Result<ServerCapabilities>`

### 2. `src/mcp.rs` — config-driven transport selection

Extend `McpServerConfig` to support a `transport` field:

```rust
pub struct McpServerConfig {
    pub command: Option<String>,      // for stdio
    pub args: Vec<String>,            // for stdio
    pub url: Option<String>,          // for http+sse
    pub env: HashMap<String, String>,
}
```

Selection logic:
- If `url` is set → use `HttpSseTransport`
- If `command` is set → use `StdioTransport` (current behavior)
- If both set → prefer `url` (warn on stderr)
- If neither → error

### 3. MCP config file support

The config should support:
```json
{
  "mcp_servers": {
    "local-fs": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
    },
    "remote-tools": {
      "url": "http://remote-host:8080/mcp"
    }
  }
}
```

### 4. Tests

- Test: `HttpSseTransport` parses a mock SSE response correctly
- Test: `HttpSseTransport` handles connection errors gracefully
- Test: config with `url` selects HttpSseTransport
- Test: config with `command` selects StdioTransport (regression)
- Test: config with both prefers `url` with warning
- Test: SSE event parsing handles multi-line data fields
- Test: timeout on SSE stream (configurable, default 30s)

**Note**: Use a mock HTTP server (e.g. `mockito` or a simple `tokio::net::TcpListener`)
for tests. Do NOT require a real MCP server.

### 5. No dependency additions if possible

Use `reqwest` (already in deps) for HTTP. For SSE parsing, implement a
simple line-by-line parser (`data:` prefix stripping) rather than adding
an SSE crate. The MCP SSE format is simple enough.

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all -- --check` clean
- HttpSseTransport compiles and passes unit tests with mock server
- Existing stdio MCP tests still pass (no regression)
- No new dependencies beyond what's already in Cargo.toml

## Notes for the agent

- Read `src/mcp.rs` for the current stdio transport implementation.
  Look for `StdioTransport` or equivalent struct/impl.
- The SSE format is simple: lines starting with `data: ` contain the
  JSON payload. Lines starting with `event: ` or `id: ` can be ignored
  for now. Empty line separates events.
- Use `reqwest::Client::post(url).json(&request).send()` for the request.
- For SSE response: read the response body as a stream, parse line by line.
  `reqwest::Response::bytes_stream()` or `.text()` depending on whether
  you need streaming.
- For mock tests: use `tokio::net::TcpListener` bound to 127.0.0.1:0,
  manually write HTTP response headers + SSE body. This avoids adding
  mockito as a dependency.
- Timeout: use `tokio::time::timeout` wrapping the SSE read loop.
