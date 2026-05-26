# Goal 81 — MCP Server: CLI `serve` Subcommand

**Roadmap**: Phase 6.3 part 3/3 — CLI integration

**Design principle check**:
- Implemented as: new Clap subcommand in `src/main.rs`. Minimal.

## Why

The protocol layer (g79) and IO loop (g80) are done. This goal wires
them into the CLI so users can run `recursive serve`.

## Scope (do exactly this, no more)

### 1. `src/main.rs` — add `serve` subcommand

In the existing Clap Args enum, add:

```rust
/// Start as an MCP server (stdio transport)
Serve {
    /// Workspace path for tool sandboxing
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
}
```

### 2. Handler

```rust
Commands::Serve { workspace } => {
    let workspace = std::fs::canonicalize(&workspace)?;
    let config = Config::from_env();
    let tools = build_tools(&config, &workspace);  // reuse existing tool builder
    let runner = McpServerRunner::new(tools);
    runner.run().await?;
}
```

### 3. That's it

This goal is intentionally tiny. Just wire the subcommand.

### 4. Tests

- Test: CLI parses `serve` subcommand without errors
- Test: `serve --workspace /tmp` sets the workspace correctly

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets -- -D warnings` clean
- `recursive serve --help` shows the subcommand
- `echo '{"jsonrpc":"2.0","id":1,"method":"initialize"}' | recursive serve` responds

## Notes for the agent

- Read `src/main.rs` for the existing Clap subcommand structure.
- Read `src/mcp_server.rs` for `McpServerRunner::new()` and `run()`.
- The `build_tools` function (or equivalent) already exists in main.rs
  for the `run` command. Reuse it.
- This goal should be ~20-30 LOC. If you're writing more, you're
  overcomplicating it.
