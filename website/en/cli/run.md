# recursive run

Execute a single goal and exit.

```bash
recursive run [OPTIONS] <GOAL>
```

## Arguments

| Argument | Description |
|---|---|
| `<GOAL>` | The goal string to pass to the agent |

## Options

| Option | Default | Description |
|---|---|---|
| `--workspace <path>` | cwd | Filesystem sandbox root |
| `--max-steps <n>` | `32` | Step budget |
| `--session <id>` | *(new)* | Resume an existing session |
| `--system-prompt-file <path>` | *(built-in)* | Custom system prompt |
| `--json` | off | Output result as JSON |

## Examples

```bash
# Basic usage
recursive run "list files in src/ and summarise the architecture"

# With a specific workspace
recursive run --workspace /my/project "review the recent changes"

# Resume a session
recursive run --session abc123 "continue where we left off"

# JSON output
recursive run --json "what is 2+2" | jq .finish_reason
```

## Exit codes

| Code | Meaning |
|---|---|
| `0` | `FinishReason::NoMoreToolCalls` or `FinishReason::ProviderStop` |
| `1` | Runtime error (returned as `Err(...)`) |
| `2` | `FinishReason::BudgetExceeded` |
| `3` | `FinishReason::Stuck` |
