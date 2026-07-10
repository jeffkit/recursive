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
| `--json` | off | Claude-compatible single result object (alias for `--output-format json`) |
| `--output-format <fmt>` | `text` | `text` \| `json` \| `stream-json` \| `recursive-json` |
| `--input-format <fmt>` | `text` | `text` \| `stream-json` (accept Claude NDJSON on stdin: `user` + `control_*`) |

## Examples

```bash
# Basic usage
recursive run "list files in src/ and summarise the architecture"

# With a specific workspace
recursive run --workspace /my/project "review the recent changes"

# Resume a session
recursive run --session abc123 "continue where we left off"

# Claude-compatible JSON (single result object)
recursive run --output-format json "what is 2+2" | jq -r '.result'

# Claude-compatible NDJSON event stream
recursive -p "what is 2+2" --output-format stream-json | jq -c 'select(.type=="result")'

# Bidirectional control (host answers can_use_tool / interrupt / set_* on stdin)
# Host writes control_request / control_response NDJSON; CLI emits the same on stdout.
recursive -p "edit src/main.rs" --output-format stream-json --input-format stream-json
```

When `--output-format` is `json` or `stream-json` and the run is not headless,
Recursive opens a Claude-compatible control channel on stdin/stdout
(`control_request` / `control_response`). Host→CLI subtypes include
`interrupt`, `initialize`, `set_permission_mode`, `set_model`, `get_*`,
`read_file`, MCP admin, `reload_skills`, `rewind_files`, and others.
CLI→host subtypes include `can_use_tool` (permission prompts),
`request_user_dialog` (plan approval), `hook_callback` (SDK hooks registered
via `initialize.hooks`), and `elicitation` (MCP `-32042`).

With `--input-format stream-json`, stdin may also carry `type: "user"`
messages; after the initial goal turn finishes, Recursive drains them as
follow-up turns until stdin EOF or interrupt.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | `FinishReason::NoMoreToolCalls` or `FinishReason::ProviderStop` |
| `1` | Runtime error (returned as `Err(...)`) |
| `2` | `FinishReason::BudgetExceeded` |
| `3` | `FinishReason::Stuck` |
