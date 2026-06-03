# recursive http

Start the HTTP API server.

```bash
recursive http [OPTIONS]
```

## Description

Starts an axum-based HTTP server exposing a REST API with sessions and SSE streaming. The server is stateless in its default configuration; add Redis and S3 for horizontal scaling.

## Options

| Option | Default | Description |
|---|---|---|
| `--addr <addr>` | `127.0.0.1:3000` | Bind address |
| `--workspace <path>` | cwd | Filesystem sandbox root for all sessions |
| `--auth-keys <keys>` | *(open)* | Comma-separated `X-API-Key` allowlist |

## Quick start

```bash
recursive http --addr 127.0.0.1:3000
```

Then in another terminal:

```bash
# Health check
curl http://localhost:3000/health

# Create a session
SESSION=$(curl -sX POST http://localhost:3000/sessions \
  -H 'Content-Type: application/json' \
  -d '{"system_prompt":"You are a helpful assistant."}' | jq -r .session_id)

# Send a message (streaming)
curl -N http://localhost:3000/sessions/$SESSION/run \
  -H 'Content-Type: application/json' \
  -d '{"message":"List the files in /workspace"}'
```

## See also

- [HTTP API reference](../http-api/) — full endpoint documentation
- [Deployment guide](../deployment/) — Docker, Redis, S3
