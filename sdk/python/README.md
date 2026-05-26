# recursive-client

Python SDK for the Recursive Agent HTTP API.

## Install

```bash
pip install -e .
```

## Usage

```python
from recursive_client import RecursiveClient

client = RecursiveClient("http://127.0.0.1:3000")

# Health check
print(client.health())  # "ok"

# List available tools
tools = client.list_tools()
for tool in tools:
    print(f"{tool.name}: {tool.description}")

# Run agent with a goal (one-shot)
result = client.run("Write hello world to hello.txt")
print(result.status, result.finish_reason)
print(f"Steps: {result.usage.total_steps}, Tokens: {result.usage.total_tokens}")

# Multi-turn sessions
session_id = client.create_session(system_prompt="You are helpful.")
response = client.send_message(session_id, "What files are in the current dir?")
print(response.content)

# List and inspect sessions
for s in client.list_sessions():
    print(f"Session {s.id} ({s.message_count} messages)")

detail = client.get_session(session_id)
print(detail.messages)

# Clean up
client.delete_session(session_id)
```
