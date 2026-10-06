# Sessions API

Sessions persist the agent transcript across multiple requests. Each session is identified by a UUID.

## Create a session

```http
POST /sessions
Content-Type: application/json

{
  "system_prompt": "You are a helpful assistant.",
  "workspace": "/path/to/project"
}
```

**Response**:
```json
{
  "session_id": "a1b2c3d4-e5f6-...",
  "created_at": "2024-01-01T00:00:00Z"
}
```

## List sessions

```http
GET /sessions
```

**Response**:
```json
[
  { "session_id": "...", "created_at": "...", "last_active": "..." },
  ...
]
```

## Get session details

```http
GET /sessions/:id
```

**Response**:
```json
{
  "session_id": "...",
  "created_at": "...",
  "last_active": "...",
  "turn_count": 5,
  "transcript": [...]
}
```

## Get session usage and cost

```http
GET /sessions/:id/usage
```

**Response**:
```json
{
  "session_id": "a1b2c3d4-e5f6-...",
  "model": "deepseek-chat",
  "prompt_tokens": 600,
  "completion_tokens": 40,
  "cache_hit_tokens": 512,
  "cache_miss_tokens": 88,
  "reasoning_tokens": 0,
  "total_tokens": 640,
  "llm_latency_ms": 1200,
  "cost_usd": 0.000025
}
```

Cumulative across the session's turns, and it survives a server restart. `cost_usd`
is summed per turn at the model that ran it (`null` while nothing could be priced),
while `model` is the model new turns are billed at — a restart onto a different
model does not reprice the history.

## Delete a session

```http
DELETE /sessions/:id
```

**Response**: `204 No Content`
