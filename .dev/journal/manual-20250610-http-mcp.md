# Manual change — HTTP channel MCP wiring

## Date
2025-06 (session)

## Goal
Issue #70: `recursive http` never registered MCP server tools, so `.mcp.json`
in the workspace was silently ignored on the AG-UI/HTTP channel — the agent's
only route to business APIs was `Bash` + curl.

## Files touched
- `crates/recursive-cli/src/main.rs` (`Cmd::Http` branch): after
  `cli::builder::build_tools`, attach an elicitation slot and call
  `cli::builder::register_mcp_tools(&mut tools, &config.workspace, cli.mcp_config, ...)`
  (same helper the CLI/TUI `run_loop`/`build_runtime` paths use), then apply
  `config.allow_tools` via `retain_tools` — matching the loop path's ordering.
  MCP tools now flow into both `tool_registry` and the `tool_infos` snapshot
  served by `GET /tools`.

## Tests added
None — wiring-only change inside `main.rs` (no unit-testable seam there;
register_mcp_tools itself is already covered by the builder test at
`cli/builder.rs:1040`). Full workspace suite passes:
`cargo test --workspace` all green, `cargo clippy --all-targets --all-features
-D warnings` clean, `cargo fmt --all` applied.

## Notes
- `GET /tools` remains a startup-time snapshot (issue text's observability
  note) — unchanged, as MCP tools are registered before the server binds.
- `--mcp-config` flag (`cli.mcp_config`) is honored for HTTP too; with no
  flag, workspace auto-discovery applies, identical to CLI semantics.
