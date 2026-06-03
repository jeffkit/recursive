# Python SDK

The Python SDK (`recursive-sdk`) provides a high-level client for the Recursive HTTP API, compatible with Claude Agent SDK patterns.

::: tip Package name
The package is published as `recursive-sdk` on PyPI. If it is not yet available, install directly from source:
```bash
pip install -e sdk/python   # from project root
```
:::

## Installation

```bash
pip install recursive-sdk
```

## Prerequisites

Start the Recursive HTTP server first:

```bash
recursive http --addr 127.0.0.1:3000
```

## Quick start — one-shot

```python
from recursive_sdk import Agent

result = Agent.prompt(
    "List the files in the current directory.",
    base_url="http://127.0.0.1:3000",
    max_steps=5,
)

print("status       :", result.status)
print("finish_reason:", result.finish_reason)
if result.result:
    print("answer       :", result.result)
```

## Multi-turn session

```python
from recursive_sdk import Agent

with Agent.create(base_url="http://127.0.0.1:3000") as agent:
    print("session:", agent.session_id)

    # First turn
    run = agent.send("What does agent.rs do?")
    for msg in run.messages():
        if msg.type == "assistant":
            print(msg.text(), end="", flush=True)
    result = run.wait()
    print(f"\n[finish: {result.finish_reason}]")

    # Second turn (same session — context preserved)
    run2 = agent.send("What are the main entry points?")
    result2 = run2.wait()
    print(result2.result)
```

## Streaming events

```python
from recursive_sdk import Agent

with Agent.create(base_url="http://127.0.0.1:3000") as agent:
    run = agent.send("Summarise src/")

    # Stream assistant text and tool calls as they arrive
    for msg in run.stream():
        if msg.type == "assistant":
            print(msg.text(), end="", flush=True)
        elif msg.type == "tool_call":
            print(f"\n[tool] {msg.name}")

    result = run.wait()
    print(f"\nDone in {result.num_turns} turns")
```

## API Reference

### `Agent` (static methods)

| Method | Description |
|---|---|
| `Agent.prompt(message, *, base_url, ...)` | One-shot: create session, send, wait, delete. Returns `RunResult`. |
| `Agent.create(*, base_url, ...)` | Create a persistent session. Use as context manager. |
| `Agent.resume(session_id, *, base_url, ...)` | Attach to an existing session. |
| `Agent.list_sessions(*, base_url, ...)` | List active sessions. |
| `Agent.delete_session(session_id, *, base_url, ...)` | Delete a session. |

### `AgentSession`

| Method | Description |
|---|---|
| `agent.send(message)` | Send a message and return a `Run`. |
| `agent.session_id` | The session ID. |

### `Run`

| Method | Description |
|---|---|
| `run.wait()` | Block until the run completes. Returns `RunResult`. |
| `run.messages()` | Iterator of streaming message events. |
| `run.stream()` | Same as `messages()`. |

### `RunResult`

| Attribute | Type | Description |
|---|---|---|
| `status` | `str` | `"finished"` \| `"error"` \| `"cancelled"` |
| `finish_reason` | `str \| None` | Rust `FinishReason` debug string |
| `result` | `str \| None` | Concatenated final assistant text |
| `usage` | `UsageMeta \| None` | Token usage stats |
| `num_turns` | `int` | Number of assistant turns |
| `ok` | `bool` | `True` when `status == "finished"` |
| `subtype` | `str` | Claude Agent SDK-compatible label (`"success"`, `"error_max_turns"`, etc.) |
