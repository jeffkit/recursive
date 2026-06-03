# HTTP API Overview

Recursive includes an axum-based HTTP server (`recursive http`) that exposes a REST API with sessions and SSE streaming.

## Starting the server

```bash
recursive http --addr 127.0.0.1:3000
```

## Base URL

```
http://localhost:3000
```

## Authentication

By default the API is open. To require an API key:

```bash
recursive http --auth-keys key1,key2,key3
```

Then pass `X-API-Key: key1` in request headers.

## Endpoints summary

| Method | Path | Description |
|---|---|---|
| `GET` | `/health` | Health check |
| `GET` | `/tools` | List registered tools |
| `POST` | `/run` | Stateless single-shot run |
| `POST` | `/sessions` | Create a new session |
| `GET` | `/sessions` | List sessions |
| `GET` | `/sessions/:id` | Get session details |
| `DELETE` | `/sessions/:id` | Delete a session |
| `POST` | `/sessions/:id/run` | Send a message (SSE streaming) |
| `GET` | `/openapi.json` | OpenAPI 3.0 spec |

## Quick start

```bash
# Start server
recursive http &

# Health check
curl http://localhost:3000/health
# → "ok"

# Create session
SESSION=$(curl -sX POST http://localhost:3000/sessions \
  -H 'Content-Type: application/json' \
  -d '{"system_prompt":"You are a helpful Rust assistant."}' \
  | jq -r .session_id)

# Run (streaming)
curl -N -X POST http://localhost:3000/sessions/$SESSION/run \
  -H 'Content-Type: application/json' \
  -d '{"message":"List the files in /workspace"}'
```

See the sub-pages for detailed documentation on each endpoint group.
