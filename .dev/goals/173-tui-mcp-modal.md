# Goal 173 — TUI /mcp modal: list & toggle MCP servers

**Roadmap**: Phase 14 — TUI Polish (part 2/3)

**Design principle check**:
- Implemented as: new Modal variant + new command; no agent loop changes
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

Recursive has a full MCP client (`src/mcp.rs`), CLI commands for MCP management,
and `~/.recursive/mcp.json` config — but the TUI has no entry point to inspect
which MCP servers are configured. A `/mcp` command that pops a read-only list
modal completes the discoverability gap vs fake-cc's `MCPServerApprovalDialog`.

## Scope (do exactly this, no more)

### 1. New Modal variant in `src/tui/ui/modal.rs`

Add a new struct and variant:

```rust
#[derive(Clone, Debug, PartialEq)]
pub struct McpEntry {
    pub name: String,
    pub transport: String,   // "stdio" | "http" | "sse"
    pub enabled: bool,
}

// Add to Modal enum:
McpServers {
    entries: Vec<McpEntry>,
    selected: usize,
},
```

Add title `" MCP Servers "` in `title()`.

Render body: each entry as one line:
- `● name  (transport)` in Green if enabled
- `○ name  (transport)` in DarkGray if disabled (no entries = "No MCP servers configured")

Navigation: ↑/↓ moves `selected`, Esc closes, Enter is a no-op (read-only for now).

### 2. New event `UiEvent::McpServersLoaded` in `src/tui/events.rs`

```rust
McpServersLoaded { entries: Vec<crate::tui::ui::modal::McpEntry> },
```

### 3. New `UserAction::ListMcpServers` in `src/tui/events.rs`

```rust
ListMcpServers,
```

### 4. Wire handler in `src/tui/backend.rs`

In `worker_loop`, handle `UserAction::ListMcpServers`:

```rust
UserAction::ListMcpServers => {
    // discover_mcp_servers is async; call it here
    let workspace = config.workspace.clone();
    let tx = event_tx.clone();
    tokio::spawn(async move {
        let servers = crate::mcp::discover_mcp_servers(&workspace)
            .await
            .unwrap_or_default();
        let entries = servers.iter().map(|s| McpEntry {
            name: s.name.clone(),
            transport: detect_transport(s),
            enabled: true,  // all discovered servers are considered enabled
        }).collect();
        let _ = tx.send(UiEvent::McpServersLoaded { entries });
    });
}
```

Add a small `fn detect_transport(s: &McpServer) -> String` that returns
`"stdio"` / `"http"` based on `McpServer` fields (check `src/mcp.rs` for the struct).

### 5. Handle event in `src/tui/app.rs`

Handle `UiEvent::McpServersLoaded { entries }` in `apply_event`:

```rust
UiEvent::McpServersLoaded { entries } => {
    self.modals.push(Modal::McpServers { entries, selected: 0 });
}
```

Add key navigation for `Modal::McpServers` in `handle_modal_key_action` (same
↑/↓/Esc pattern as `ResumePicker`, but no Enter action needed).

### 6. `/mcp` command in `src/tui/commands.rs`

Add to `default_set()`:

```rust
CommandSpec {
    name: "mcp",
    aliases: &[],
    summary: "List configured MCP servers",
    usage: "/mcp",
    handler: CommandHandler::Sync(cmd_mcp),
},
```

`cmd_mcp` sends `UserAction::ListMcpServers` and returns
`CommandOutcome::Handled`.

Update the count in the `registry_includes_all_thirteen_commands` test → 14.
Also rename that test to `registry_includes_all_fourteen_commands`.

### 7. Tests

- `mcp_entry_renders_enabled`: build a `McpEntry { enabled: true, .. }` and check modal title
- `cmd_mcp_is_registered`: use `CommandRegistry::default_set()` to find `"mcp"`

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `/mcp` command appears in `/help` output (check via test or string assert)

## Notes for the agent

- `discover_mcp_servers` is `async` and lives in `crate::mcp` (feature-gated `mcp`).
  Check `src/lib.rs:66-68` for the re-export. The `tui` binary already enables the
  `mcp` feature — check `Cargo.toml` features section to confirm; if not, add it.
- `McpServer` fields: read `src/mcp.rs:51` — fields include `name: String`,
  `command: Option<String>`, `url: Option<String>`. If `command` is Some → stdio;
  if `url` is Some → http/sse.
- The `mcp` feature may not be enabled in the `tui` feature. If `discover_mcp_servers`
  is unavailable, fall back to `load_mcp_config` on the config path
  `~/.recursive/mcp.json` (use `crate::paths::config_dir()` + `"mcp.json"`).
- **DO NOT modify**: `src/tui/ui/transcript.rs`, `src/tui/ui/markdown.rs`,
  `src/tui/ui/theme.rs`, or any file outside
  `src/tui/events.rs`, `src/tui/ui/modal.rs`, `src/tui/backend.rs`,
  `src/tui/app.rs`, `src/tui/commands.rs`.
