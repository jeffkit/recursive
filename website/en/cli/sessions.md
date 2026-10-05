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

`recursive http` persists transcripts, memory entries and per-session metadata to S3 when the `cloud-runtime` feature is compiled in and `RECURSIVE_S3_BUCKET` is set — each turn's transcript growth is written on turn end, so a crash loses at most the in-flight turn, and `GET /sessions/:id` cold-loads it on a restart. S3 has no native append, so each turn rewrites the whole object. Redis is not consumed by `recursive http` (library API only).
