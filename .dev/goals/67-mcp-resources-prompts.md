# Goal 67 — MCP Resource and Prompt Support

**Roadmap**: Phase 6.4 — MCP resource/prompt support (beyond tools)

**Design principle check**:
- Implemented as: extension to `src/mcp.rs` (client protocol support).
  No agent loop changes.
- Does NOT modify `agent.rs`.

## Why

MCP defines three capability types: **tools** (already implemented),
**resources** (data the server exposes for the client to read), and
**prompts** (pre-built prompt templates). Currently we only support
tools. Adding resource and prompt support makes the MCP client
spec-complete for the three core primitives.

Resources are particularly useful: a server can expose file contents,
database schemas, or API documentation as resources that the agent can
read without calling a tool.

## Scope (do exactly this, no more)

### 1. `src/mcp.rs` — add resource types + list/read methods

```rust
#[derive(Debug, Clone)]
pub struct McpResource {
    pub uri: String,
    pub name: String,
    pub description: Option<String>,
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct McpResourceContent {
    pub uri: String,
    pub mime_type: Option<String>,
    pub text: Option<String>,
    pub blob: Option<String>, // base64 for binary
}
```

Add methods to `McpClient`:
- `pub async fn list_resources(&mut self) -> Result<Vec<McpResource>>`
- `pub async fn read_resource(&mut self, uri: &str) -> Result<McpResourceContent>`

JSON-RPC methods:
- `resources/list` → returns `{ resources: [...] }`
- `resources/read` → takes `{ uri: "..." }`, returns `{ contents: [...] }`

### 2. `src/mcp.rs` — add prompt types + list/get methods

```rust
#[derive(Debug, Clone)]
pub struct McpPrompt {
    pub name: String,
    pub description: Option<String>,
    pub arguments: Vec<McpPromptArgument>,
}

#[derive(Debug, Clone)]
pub struct McpPromptArgument {
    pub name: String,
    pub description: Option<String>,
    pub required: bool,
}

#[derive(Debug, Clone)]
pub struct McpPromptMessage {
    pub role: String, // "user" or "assistant"
    pub content: String,
}
```

Add methods to `McpClient`:
- `pub async fn list_prompts(&mut self) -> Result<Vec<McpPrompt>>`
- `pub async fn get_prompt(&mut self, name: &str, args: HashMap<String, String>) -> Result<Vec<McpPromptMessage>>`

JSON-RPC methods:
- `prompts/list` → returns `{ prompts: [...] }`
- `prompts/get` → takes `{ name: "...", arguments: {...} }`, returns `{ messages: [...] }`

### 3. Update `McpClient::initialize` response parsing

The server's `initialize` response includes a `capabilities` object that
tells us which features are supported:

```json
{
  "capabilities": {
    "tools": {},
    "resources": {},
    "prompts": {}
  }
}
```

Parse and store which capabilities the server advertises. The new
`list_resources`/`list_prompts` methods should return an error (or
empty list) if the server didn't advertise that capability.

### 4. Tests

- Test: `list_resources` sends correct JSON-RPC and parses response
- Test: `read_resource` sends correct params and parses content
- Test: `list_prompts` sends correct JSON-RPC and parses response
- Test: `get_prompt` with arguments sends correct params
- Test: methods return error when server doesn't advertise capability
- Test: resource with binary content (blob field)
- Test: prompt with multiple messages in response

Use mock TCP server pattern (same as existing MCP tests).

### 5. Re-export new types from `src/lib.rs`

Add `McpResource`, `McpResourceContent`, `McpPrompt`, `McpPromptMessage`
to the public API.

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all -- --check` clean
- New methods work correctly in unit tests with mocked responses
- No regressions on existing MCP tool tests

## Notes for the agent

- Read `src/mcp.rs` for the current `McpClient` implementation.
- Look at how `list_tools` and `call_tool` do JSON-RPC — follow the
  same pattern for the new methods.
- The JSON-RPC pattern is: build a request with method + params,
  send via transport, parse the response result field.
- For the mock tests, look at existing MCP test functions that create
  mock stdin/stdout pipes with scripted JSON-RPC responses.
- The `initialize` response already has a `capabilities` field — check
  if it's being parsed. If not, add parsing for it.
- Don't forget to add the new types to `src/lib.rs` re-exports.
