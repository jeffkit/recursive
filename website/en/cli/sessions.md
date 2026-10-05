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

`recursive http` persists transcripts, memory entries and per-session metadata to S3 when the `cloud-runtime` feature is compiled in and `RECURSIVE_S3_BUCKET` is set — written on session teardown and cold-loaded by `GET /sessions/:id`. Redis is not consumed by `recursive http` yet (library API only).
