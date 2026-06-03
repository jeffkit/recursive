# recursive sessions

Manage persisted sessions.

```bash
recursive sessions <SUBCOMMAND>
```

## Subcommands

| Subcommand | Description |
|---|---|
| `list` | List all saved sessions |
| `show <id>` | Show details of a session |
| `delete <id>` | Delete a session |
| `rewind <id> --to-turn <n>` | Rewind a session to a specific turn |

## Examples

```bash
# List sessions
recursive sessions list

# Show a session
recursive sessions show abc123

# Rewind to turn 5
recursive sessions rewind abc123 --to-turn 5

# Delete a session
recursive sessions delete abc123
```

## Session storage

Sessions are stored as JSONL files in `~/.recursive/sessions/` by default.

With the `cloud-runtime` feature and Redis configured, sessions are stored in Redis and replicated to S3.
