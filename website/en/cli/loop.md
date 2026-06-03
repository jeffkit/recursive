# recursive loop

Self-scheduling autonomous loop mode.

```bash
recursive loop [OPTIONS] <GOAL>
```

## Description

Loop mode runs the agent in a continuous cycle. After completing a goal, the agent can schedule its next wakeup time, making it suitable for monitoring, periodic tasks, and self-improving workflows.

The agent receives a special `schedule_wakeup` tool that lets it set how long to sleep before being invoked again.

## Options

| Option | Default | Description |
|---|---|---|
| `--workspace <path>` | cwd | Filesystem sandbox root |
| `--max-steps <n>` | `32` | Step budget per iteration |
| `--interval <secs>` | *(agent decides)* | Override wakeup interval |
| `--max-iterations <n>` | unlimited | Stop after N iterations |

## Example

```bash
# Monitor a directory and report changes
recursive loop "monitor src/ for changes and summarize what changed since last run"

# Self-improvement loop (used by the Recursive project itself)
recursive loop "read .dev/goals/ and implement the next unfinished goal"
```

## Loop state

Between iterations, the agent can persist state using the `remember` and `recall` tools. This allows it to track what it has done and what remains.
