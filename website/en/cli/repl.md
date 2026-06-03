# recursive repl

Interactive REPL — one goal per line.

```bash
recursive repl [OPTIONS]
```

## Description

Starts an interactive session where each line you type is treated as a new goal. The agent runs to completion, prints the result, then waits for the next goal.

Session state (transcript, memory) is preserved across goals within the same REPL session.

## Options

| Option | Default | Description |
|---|---|---|
| `--workspace <path>` | cwd | Filesystem sandbox root |
| `--max-steps <n>` | `32` | Step budget per goal |
| `--session <id>` | *(new)* | Resume an existing session |

## REPL commands

| Command | Description |
|---|---|
| `:q` or `:quit` | Exit the REPL |
| `:clear` | Clear the transcript (start fresh) |
| `:session` | Print the current session ID |
| `:tools` | List available tools |

## Example

```
$ recursive repl
Recursive REPL — type a goal, :q to exit
> list the files in src/
[tool: list_dir] ...
The src/ directory contains: agent.rs, lib.rs, tools/, llm/, ...

> explain what agent.rs does
[tool: read_file] ...
agent.rs implements the ReAct loop. It alternates between...

> :q
Goodbye.
```
