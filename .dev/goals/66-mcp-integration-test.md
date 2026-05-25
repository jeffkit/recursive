# Goal 66 — MCP Real Server Integration Test

**Roadmap**: Phase 6.2 — MCP real server integration test (filesystem server)

**Design principle check**:
- Implemented as: new integration test file. No product code changes
  unless a bug surfaces during testing.
- Does NOT modify `agent.rs`.

## Why

MCP has 1400+ LOC with unit tests against mocks. But no test actually
spawns a real MCP server and verifies end-to-end communication. The
`@modelcontextprotocol/server-filesystem` is the canonical reference
server — testing against it proves the protocol implementation works
for real, not just against our own mocks.

## Scope (do exactly this, no more)

### 1. `tests/mcp_integration.rs` — new integration test file

Write tests that:
1. Check if `npx` is available (skip test with a clear message if not)
2. Spawn `npx -y @modelcontextprotocol/server-filesystem /tmp` as a
   real MCP server via our `McpClient::spawn()`
3. Verify: `initialize()` returns valid capabilities
4. Verify: `list_tools()` returns filesystem tools (read_file, write_file,
   list_directory, etc.)
5. Verify: `call_tool("read_file", ...)` with a real temp file returns
   its contents
6. Verify: `call_tool("write_file", ...)` creates a real file
7. Verify: cleanup (drop client kills the subprocess)

### 2. Test structure

```rust
#[tokio::test]
#[ignore] // Run with: cargo test -- --ignored (requires npx)
async fn mcp_filesystem_server_integration() {
    // Skip if npx not available
    if std::process::Command::new("npx").arg("--version").output().is_err() {
        eprintln!("SKIP: npx not available");
        return;
    }
    // ... test body
}
```

Mark as `#[ignore]` so normal `cargo test` doesn't require npx. Run
explicitly with `cargo test -- --ignored`.

### 3. Test scenarios

#### Test A: Initialize + list tools
- Spawn filesystem server on `/tmp`
- Call initialize, verify we get `ServerCapabilities`
- Call list_tools, verify at least 3 tools returned
- Verify tool names include known ones (read_file or read_file)

#### Test B: Read a real file
- Create a temp file at `/tmp/recursive-mcp-test-<random>.txt`
- Call the server's read_file tool with that path
- Verify the response contains the file content
- Clean up temp file

#### Test C: Write a file
- Call write_file tool to create `/tmp/recursive-mcp-write-test-<random>.txt`
- Verify the file exists on disk with correct content
- Clean up

#### Test D: Error handling
- Call read_file with a nonexistent path
- Verify we get a tool error (not a crash)

### 4. If bugs surface

If the integration test reveals a protocol bug (wrong JSON-RPC format,
missing field, etc.), fix it in `src/mcp.rs`. Document what was wrong
in the commit message.

## Acceptance

- `cargo test` green (integration test is `#[ignore]` so won't run)
- `cargo test -- --ignored` passes when npx is available
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- Tests prove real end-to-end MCP communication works

## Notes for the agent

- Read `src/mcp.rs` for `McpClient::spawn()`, `McpServer` struct.
- The filesystem server npm package is `@modelcontextprotocol/server-filesystem`.
- Its tool names may be: `read_file`, `write_file`, `list_directory`,
  `create_directory`, `move_file`, `search_files`, `get_file_info`.
- The server takes a list of allowed directories as args after the package
  name: `npx -y @modelcontextprotocol/server-filesystem /tmp`
- Use `tempfile` for creating test files; clean up after tests.
- The `#[ignore]` attribute means these tests won't run in CI or normal
  test passes — they require network (npm) and node.js.
- If spawn fails with a timeout, the test should fail gracefully, not hang.
  Use `tokio::time::timeout` around the spawn.
