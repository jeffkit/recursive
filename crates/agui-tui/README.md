# agui-tui

A generic terminal UI for any [AG-UI]–compatible agent. Three-pane
ratatui interface that streams text messages, tool calls, and state
updates from a remote agent over SSE.

[AG-UI]: https://docs.ag-ui.com/

## Install

```bash
cargo install --path crates/agui-tui
```

## Usage

```bash
# Connect to any AG-UI server
agui-tui http://localhost:3000/agui

# With auth
agui-tui --header 'Authorization: Bearer abc' https://my-agent.example.com/agui

# Continue an existing conversation
agui-tui --thread-id existing-thread http://localhost:3000/agui
```

## Layout

```
┌──────────────────────────────────────────┬──────────────────┐
│  Messages + tool calls (scrollable)      │  Session state   │
│                                          │  thread_id       │
│                                          │  run_id          │
│                                          │  step count      │
│                                          │  tools used      │
├──────────────────────────────────────────┤                  │
│  Permission prompt (when active)         │                  │
├──────────────────────────────────────────┴──────────────────┤
│  > input                                                     │
└──────────────────────────────────────────────────────────────┘
```

Keybindings:

| Key | Action |
|-----|--------|
| `Enter` | send the current prompt as a new run |
| `y` / `n` / `Esc` | respond to a permission prompt |
| `Tab` | toggle focus between messages pane and input bar |
| `Ctrl-C` | quit (in-flight run is dropped) |

## Local-agent extensions

The AG-UI standard doesn't yet cover a few things local agents need.
This client recognises four `Custom` event names and renders them
specially:

| Custom name | Effect |
|-------------|--------|
| `agui-tui/permission_request` | modal Y/N prompt before tool runs |
| `agui-tui/checkpoint_post` | last-checkpoint indicator in sidebar |
| `agui-tui/heartbeat` | "running 1.5s…" timer for long tools |
| `agui-tui/file_artifact` | logged for now; future "open file" affordance |

Servers that don't emit these events still work fine — the TUI just
won't show those bits of UI.

## Status

This crate is part of [recursive][repo] but doesn't depend on the
recursive runtime. It works against any AG-UI server (CopilotKit,
LangGraph, CrewAI integrations, recursive's own `/agui` endpoint, …).

The official AG-UI repo has community-maintained Rust crates that
are not yet published to crates.io. See
[`agui-protocol`'s README](../agui-protocol/README.md) for why this
crate uses its own protocol layer instead.

[repo]: https://github.com/jeffkit/recursive

## License

MIT.
