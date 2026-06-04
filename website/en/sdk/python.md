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

## Session options

Both `Agent.create()` and `Agent.prompt()` accept these optional keyword arguments in addition to `base_url`, `api_key`, and `timeout`:

| Parameter | Type | Description |
|-----------|------|-------------|
| `system_prompt` | `str` | Replace the server's default system prompt entirely. |
| `append_system_prompt` | `str` | Append to the default system prompt (ignored if `system_prompt` is set). |
| `session_name` | `str` | Human-readable display name for the session (`create` only). |
| `max_steps` | `int` | Maximum number of agent steps allowed. |
| `planning_mode` | `"immediate"` \| `"plan_first"` | `"plan_first"` buffers tool calls and shows a plan before executing. |
| `thinking_budget` | `int` | Extended-thinking token budget (Anthropic models). Pass `0` to disable. |
| `permission_mode` | `"default"` \| `"auto"` \| `"strict"` \| `"bypass"` | Tool-call permission enforcement level. |
| `max_budget_usd` | `float` | Maximum API spend in USD for this session / run. |

Example — Plan Mode + named session:

```python
with Agent.create(
    base_url="http://localhost:3000",
    session_name="refactor-auth",
    planning_mode="plan_first",
    max_steps=20,
) as agent:
    run = agent.send("Refactor the auth module to use JWTs")
    run.wait()
```

Example — append extra instructions without losing the default prompt:

```python
result = Agent.prompt(
    "Fix all failing tests",
    base_url="http://localhost:3000",
    append_system_prompt="\nAlways run `cargo test` to verify before finishing.",
)
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
