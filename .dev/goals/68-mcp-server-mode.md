# Goal 68 — MCP Server Mode (Recursive as MCP Server)

**Roadmap**: Phase 6.3 — MCP Server mode (expose Recursive as MCP server)

**Design principle check**:
- Implemented as: new module `src/mcp_server.rs` + CLI subcommand.
  Does NOT modify the agent loop or existing MCP client.
- `agent.rs` unchanged.

## Why

Currently Recursive can only CONSUME MCP servers. Making Recursive
itself an MCP server means other tools (Claude Code, VS Code extensions,
other agents) can use Recursive's tools through the MCP protocol. This
is the natural counterpart to the client: expose our ToolRegistry as
MCP-accessible tools.

## Scope (do exactly this, no more)

### 1. `src/mcp_server.rs` — new module

Create an MCP server that:
- Accepts JSON-RPC 2.0 over **stdio** (stdin/stdout, matching the MCP
  stdio transport spec)
- Implements the server-side protocol:
  - `initialize` → respond with our capabilities (tools list)
  - `tools/list` → enumerate tools from a ToolRegistry
  - `tools/call` → execute a tool and return the result
  - `notifications/initialized` → acknowledge (no response needed)

```rust
pub struct McpServerRunner {
    tools: ToolRegistry,
    workspace: PathBuf,
}

impl McpServerRunner {
    pub fn new(tools: ToolRegistry, workspace: PathBuf) -> Self { ... }

    /// Run the server loop: read JSON-RPC from stdin, dispatch, write to stdout.
    pub async fn run(&self) -> Result<()> { ... }
}
```

### 2. JSON-RPC server loop

The server loop:
1. Read one line from stdin (each JSON-RPC message is newline-delimited)
2. Parse as JSON-RPC request
3. Dispatch by method:
   - `initialize` → return `{ capabilities: { tools: {} }, serverInfo: {...} }`
   - `tools/list` → return `{ tools: [ToolSpec → McpToolSpec format] }`
   - `tools/call` → execute tool, return `{ content: [{type:"text", text:"..."}] }`
4. Write JSON-RPC response to stdout (one line)
5. Repeat until EOF on stdin

### 3. `src/main.rs` — add `serve` subcommand

```
recursive serve [--workspace <path>]
```

This starts the MCP server mode. It:
- Builds the default ToolRegistry (same tools available in agent mode)
- Creates `McpServerRunner`
- Calls `runner.run().await`

The user can then connect to it from another MCP client:
```json
{
  "mcp_servers": {
    "recursive": {
      "command": "recursive",
      "args": ["serve", "--workspace", "/path/to/project"]
    }
  }
}
```

### 4. Error handling

- Invalid JSON → respond with JSON-RPC error (-32700 Parse error)
- Unknown method → respond with JSON-RPC error (-32601 Method not found)
- Tool execution error → respond with tool result containing error text
  (NOT a JSON-RPC error — tool errors are successful responses with
  `isError: true` in the content)

### 5. Tests

- Test: `initialize` response has correct format
- Test: `tools/list` returns tools matching the registry
- Test: `tools/call` executes a tool and returns result
- Test: `tools/call` with unknown tool returns isError content
- Test: invalid JSON returns parse error
- Test: unknown method returns method-not-found error
- Test: server handles multiple sequential requests

For tests: simulate stdin/stdout with `tokio::io::duplex` or
byte buffers. Don't need real subprocess.

### 6. `src/lib.rs` — re-export

Add `pub mod mcp_server;` and export `McpServerRunner`.

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all -- --check` clean
- `recursive serve` starts and responds to JSON-RPC on stdio
- Can be used as an MCP server by another client

## Notes for the agent

- Read `src/mcp.rs` for the CLIENT-side protocol handling — the server
  is the mirror image. What we send as requests, the server receives.
  What servers send back, we now need to generate.
- Read `src/tools/mod.rs` for `ToolRegistry` and how tools are enumerated
  and executed.
- Read `src/main.rs` for how CLI subcommands are structured (Clap).
- The stdio JSON-RPC format: one JSON object per line, no framing beyond
  newline. Use `tokio::io::BufReader` on stdin, `writeln!` on stdout.
- Important: stdout must ONLY contain JSON-RPC messages. Any debug output
  goes to stderr. Use `eprintln!` for logging, never `println!`.
- For `tools/list` response format:
  ```json
  { "tools": [{ "name": "...", "description": "...", "inputSchema": {...} }] }
  ```
  Map our `ToolSpec` (name, description, parameters) to this format.
  `parameters` becomes `inputSchema`.
- For `tools/call` request format:
  ```json
  { "name": "read_file", "arguments": { "path": "/tmp/foo" } }
  ```
  Response:
  ```json
  { "content": [{ "type": "text", "text": "file contents here" }] }
  ```
- This is an L-effort goal. Budget ~80-100 steps. The key complexity is
  getting the JSON-RPC framing and response format exactly right.
