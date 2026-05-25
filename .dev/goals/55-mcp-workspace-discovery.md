# Goal 55 — MCP Workspace Discovery

**Roadmap**: follow-up — MCP server auto-discovery from workspace

**Design principle check**:
- Implemented as: extension to existing `src/mcp.rs` module. Reads
  `.mcp.json` from workspace root if no explicit MCP config is provided.
- Does NOT branch inside `agent.rs::Agent::run`'s main loop.

## Why

The current MCP client (goal-35) reads server configuration from a
CLI-specified config file or env var. Real-world projects (Claude Code,
Cursor) use a workspace-level `.mcp.json` for project-specific MCP
servers. This allows per-project tool discovery without global config.

## Scope (do exactly this, no more)

### 1. `src/mcp.rs` — add workspace discovery

Add a function:
```rust
pub async fn discover_mcp_servers(workspace: &Path) -> Result<Vec<McpServerConfig>>
```

Logic:
1. Look for `<workspace>/.mcp.json` (highest priority)
2. Look for `<workspace>/.recursive/mcp.json` (alternative location)
3. If neither exists, return empty vec (no error)
4. Parse the JSON file into `Vec<McpServerConfig>` (reuse existing struct)

The `.mcp.json` format (compatible with Claude Code):
```json
{
  "mcpServers": {
    "server-name": {
      "command": "path/to/server",
      "args": ["--flag"],
      "env": { "KEY": "value" }
    }
  }
}
```

### 2. Integration in startup

In `src/main.rs` (or wherever MCP servers are initialized):
- If no explicit `--mcp-config` is provided, auto-discover from workspace
- Discovered servers are started alongside any explicitly configured ones
- Log which servers were auto-discovered vs explicitly configured

### 3. Tests

- Test: `discover_mcp_servers` finds `.mcp.json` in workspace root
- Test: returns empty vec when no config file exists
- Test: parses standard Claude Code `.mcp.json` format
- Test: `.recursive/mcp.json` is found as fallback
- Test: malformed JSON returns descriptive error

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `recursive run` in a directory with `.mcp.json` auto-discovers servers
- No breaking changes — explicit config still works, discovery is additive
- No new dependencies

## Notes for the agent

- Read `src/mcp.rs` to understand `McpServerConfig` and how servers are
  currently started.
- The discovery function should NOT start servers — just return configs.
  The caller (main.rs) starts them as before.
- Use `tokio::fs::read_to_string` + `serde_json::from_str` for parsing.
- Handle gracefully: file exists but is empty, file has extra unknown
  fields (use `#[serde(flatten)]` or ignore unknown), file has no
  `mcpServers` key.
