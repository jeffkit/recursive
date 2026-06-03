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

## Delete a session

```http
DELETE /sessions/:id
```

**Response**: `204 No Content`
