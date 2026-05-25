# Goal 35 — MCP Client v1 (stdio transport, tool proxy)

**Roadmap**: 2.1 — MCP Client (Critical / L)

**Design principle check**:
- Implemented as: **new module** `src/mcp.rs` (client + stdio
  transport) + **N new Tools** dynamically generated at agent
  startup from MCP server's `list_tools` response, registered into
  the existing tool table. The agent loop is **unchanged** — MCP
  servers' tools look identical to built-in tools from `Agent::run`'s
  perspective.
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop. New
  tools enter via the existing `ToolBox` registration mechanism.

## Why

MCP (Model Context Protocol) is the emerging standard for plugging
external capability servers into LLM agents. Claude Code, Codex,
Hermes, Cursor — all consume MCP servers. Without MCP, every new
capability requires native Rust code; with MCP, users plug in
filesystem-mcp, github-mcp, postgres-mcp, etc. for free.

This is the single highest-impact item in Phase 2 and a prerequisite
for the "ecosystem connector" vision in ROADMAP.md.

## Scope

**Bounded subset for v1**: stdio transport, JSON-RPC 2.0 over
line-delimited JSON, `list_tools` + `call_tool` only. No prompts /
resources / sampling. No SSE / WebSocket / HTTP transport. No
notification handling (request/response only).

Touches: new `src/mcp.rs`, `src/main.rs` (server discovery +
registration), `src/lib.rs` (re-export).

### 1. New module `src/mcp.rs`

- `pub struct McpServer { cmd: String, args: Vec<String>, name:
  String }` — config for a single MCP server.
- `pub struct McpClient { stdin, stdout, next_id: u64 }` — owns a
  spawned `tokio::process::Child` and its stdio handles.
- `impl McpClient`:
  - `pub async fn spawn(server: &McpServer) -> Result<Self>`: spawn
    child, send `initialize` request (protocolVersion `2024-11-05`,
    capabilities `{}`), expect `initialize` response, send
    `initialized` notification.
  - `pub async fn list_tools(&mut self) -> Result<Vec<McpToolSpec>>`
    where `McpToolSpec { name, description, input_schema }` maps
    directly to our `ToolSpec`.
  - `pub async fn call_tool(&mut self, name: &str, arguments:
    serde_json::Value) -> Result<String>` returns the textual
    content of the `content[0].text` block. Error if `isError: true`.
  - Drop impl: terminate child.
- `pub struct McpTool { client: Arc<Mutex<McpClient>>, spec:
  McpToolSpec }` — implements our `Tool` trait, delegates to
  `client.call_tool(spec.name, args)`.

### 2. In `src/main.rs`

- New CLI flag (or env): `RECURSIVE_MCP_CONFIG=path/to/mcp.json` or
  `--mcp-config <path>`. Pick env-var-only for v1 (simpler).
- Config file format (minimal):
  ```json
  {
    "servers": [
      { "name": "fs", "command": "mcp-server-filesystem", "args": ["--root", "."] },
      { "name": "github", "command": "mcp-server-github", "args": [] }
    ]
  }
  ```
- At agent startup (after `build_tools`):
  1. Parse config. Spawn one `McpClient` per server.
  2. Call `list_tools` on each.
  3. For each tool, register an `McpTool` with name
     `mcp__<server_name>__<tool_name>` (double-underscore namespacing
     prevents collisions with built-ins). Description prefixed with
     `[mcp:<server_name>] `.
- If no config or empty `servers`, behavior is unchanged.

### 3. Tests in `src/mcp.rs`

- **Test A**: Spawn a mock MCP server. Easiest: write a tiny Rust
  binary in `tests/fixtures/mock-mcp/` that reads JSON-RPC from stdin
  and echoes canned responses. Or use a shell script with `cat <<EOF`.
  Verify `initialize` handshake + `list_tools` + `call_tool`.
- **Test B**: malformed server (non-JSON line on stdout) errors
  cleanly without panicking the agent.
- **Test C**: `McpTool` round-trip — registered with a mock client,
  `Tool::run` returns the expected output.

## Acceptance

- `cargo build` green.
- `cargo test` green (target: 140 baseline + 3 new = 143+, plus
  whatever batch-12 adds).
- `cargo clippy --all-targets -- -D warnings` green.
- `cargo fmt --all` clean.
- No MCP config → existing behavior preserved.
- Real-world smoke check (manual, post-merge): point at
  `npx @modelcontextprotocol/server-filesystem .` and observe
  `list_tools` returning ~7 filesystem tools.

## Notes for the agent

- This is THE largest goal since g04. Budget aggressively: expect
  100+ steps. Auto-resume is enabled, you have up to ~200 effective
  steps.
- JSON-RPC 2.0 line framing: each message is `{...}\n`, no
  Content-Length headers (that's HTTP-transport variant). Just
  newline-delimited JSON.
- Reference (read once): https://modelcontextprotocol.io/specification/2025-03-26
  — but you don't need to implement the full surface, only:
  `initialize` / `initialized` / `tools/list` / `tools/call`.
- The `McpTool` implementing `Tool` trait needs interior mutability
  for the client. `Arc<Mutex<McpClient>>` is the easiest path.
  `tokio::sync::Mutex` (not std `Mutex`), because `call_tool` is
  async.
- DO NOT mock the JSON-RPC layer. Test with a real subprocess (even
  if the subprocess is a 30-line shell script). Mocking transports
  is how integration bugs slip through.
- **MANDATORY** explicit timeouts on the JSON-RPC read loop —
  `tokio::time::timeout(Duration::from_secs(10), ...)` around the
  stdout read so a hung server doesn't lock the agent forever. This
  is the AGENTS.md section 5 lesson applied to subprocess IO instead
  of HTTP.
- Use `apply_patch`. `.to_string()` over `.into()` in tests.
- This goal touches `main.rs` — coordinate carefully if any other
  batch-13 goal also touches it (g36 project-context-file does).
