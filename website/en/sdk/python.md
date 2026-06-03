# Python SDK

The Python SDK provides a lightweight client for the Recursive HTTP API.

## Installation

```bash
pip install recursive-client
```

Or for the latest development version:

```bash
cd sdk/python
pip install -e .
```

## Quick start

```python
from recursive_client import RecursiveClient

client = RecursiveClient("http://127.0.0.1:3000")

# Health check
print(client.health())  # "ok"

# Stateless run
result = client.run("list files in src/")
print(result.finish_reason)
print(result.final_message)
```

## Session management

```python
# Create a session
session = client.create_session(
    system_prompt="You are a helpful Rust assistant.",
    workspace="/path/to/project",
)
print(session.session_id)

# Send a message
result = session.run("what does agent.rs do?")
print(result.final_message)

# Send another message (conversation continues)
result = session.run("what are the main entry points?")

# Delete the session
session.delete()
```

## Streaming

```python
# Stream events in real time
for event in session.run_stream("list all tools"):
    if event.type == "tool_start":
        print(f"[tool] {event.data['name']}")
    elif event.type == "done":
        print(event.data['final_message'])
        break
```

## API Reference

### `RecursiveClient`

```python
client = RecursiveClient(
    base_url="http://localhost:3000",
    api_key=None,          # optional X-API-Key header
    timeout=60,            # request timeout in seconds
)
```

| Method | Description |
|---|---|
| `client.health()` | Returns `"ok"` if the server is healthy |
| `client.tools()` | Returns list of tool definitions |
| `client.run(message, **kwargs)` | Stateless single-shot run |
| `client.create_session(**kwargs)` | Create a new session |
| `client.list_sessions()` | List all sessions |
| `client.get_session(session_id)` | Get a session by ID |

### `Session`

| Method | Description |
|---|---|
| `session.run(message)` | Send a message, return `AgentResult` |
| `session.run_stream(message)` | Returns an iterator of `StepEvent` |
| `session.delete()` | Delete this session |

### `AgentResult`

| Attribute | Type | Description |
|---|---|---|
| `finish_reason` | `str` | `"done"`, `"budget_exceeded"`, `"stuck"`, etc. |
| `final_message` | `str \| None` | The agent's final answer |
| `steps` | `int` | Number of steps taken |
| `token_usage` | `dict \| None` | `{"prompt": N, "completion": N, "total": N}` |
