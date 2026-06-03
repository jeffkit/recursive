# CLI Overview

The `recursive` binary provides several subcommands:

| Command | Description |
|---|---|
| [`run`](./run) | Execute a single goal and exit |
| [`repl`](./repl) | Interactive REPL — one goal per line |
| [`loop`](./loop) | Self-scheduling autonomous loop mode |
| [`http`](./http) | Start the HTTP API server |
| [`tools`](./tools) | List registered tools (no API key needed) |
| [`sessions`](./sessions) | Manage persisted sessions |

## Installation

```bash
cargo install recursive-agent
```

## Global flags

| Flag | Default | Description |
|---|---|---|
| `--workspace <path>` | cwd | Filesystem sandbox root |
| `--max-steps <n>` | `32` | Step budget per run |
| `--model <name>` | `gpt-4o-mini` | Override model |
| `--api-base <url>` | `https://api.openai.com/v1` | Override endpoint |
| `--provider <name>` | `default` | Named provider profile from `providers.toml` |
| `-v / --verbose` | off | Verbose output |
