# recursive-sdk

Python SDK for the [Recursive Agent](https://github.com/recursive-agent/recursive).

API-compatible with the Claude Agent SDK / Cursor SDK patterns.

## Install

```bash
pip install recursive-sdk
```

Or from source:

```bash
pip install -e sdk/python
```

## Prerequisites

Start the Recursive server:

```bash
recursive loop --http 3000
```

Set environment (if auth is enabled):

```bash
export RECURSIVE_API_KEY=your-key
export RECURSIVE_BASE_URL=http://localhost:3000  # optional, this is the default
```

## Usage

### One-shot (`Agent.prompt`)

Fire-and-forget: send a prompt, wait for the result.

```python
from recursive_sdk import Agent

result = Agent.prompt(
    "List all TODO comments across the codebase",
    base_url="http://localhost:3000",
)

print(result.status)       # "finished" | "error" | "cancelled"
print(result.finish_reason)
if result.usage:
    print(result.usage.input_tokens, result.usage.output_tokens)
```

### Multi-turn with streaming (`Agent.create` + `agent.send`)

Create a persistent session and send multiple messages. Each `send()` returns
a `Run` that you can stream or wait on.

```python
from recursive_sdk import Agent

with Agent.create(base_url="http://localhost:3000") as agent:
    # First turn — stream tokens as they arrive
    run = agent.send("Fix all failing tests in the project")
    for msg in run.messages():
        if msg.type == "assistant":
            print(msg.text(), end="", flush=True)
    result = run.wait()
    print(f"\n[{result.status}]")

    # Second turn — just wait
    run2 = agent.send("Now update CHANGELOG.md")
    result2 = run2.wait()
```

### Resume an existing session (`Agent.resume`)

Pick up a session that was created earlier (survives restarts).

```python
from recursive_sdk import Agent

# session_id was saved from a previous run
with Agent.resume(session_id, base_url="http://localhost:3000") as agent:
    run = agent.send("Continue where we left off")
    run.wait()
```

### List sessions

```python
sessions = Agent.list_sessions(base_url="http://localhost:3000")
for s in sessions:
    print(s.id, s.message_count, s.last_prompt)
```

### Error handling

```python
from recursive_sdk import Agent, RecursiveAgentError

try:
    with Agent.create(base_url="http://localhost:3000") as agent:
        run = agent.send("do something")
        result = run.wait()
        if result.status == "error":
            print("Run failed:", result.error)  # agent ran but hit an error
except RecursiveAgentError as e:
    print("Startup failed:", e.message)         # couldn't connect / auth failed
    if e.is_retryable:
        print("(you can retry this)")
```

## API reference

### `Agent` (static factory)

| Method | Description |
|--------|-------------|
| `Agent.prompt(message, *, base_url, api_key, system_prompt, max_steps)` | One-shot run |
| `Agent.create(*, base_url, api_key, system_prompt)` | Create a new session |
| `Agent.resume(session_id, *, base_url, api_key)` | Resume existing session |
| `Agent.list_sessions(*, base_url, api_key)` | List active sessions |
| `Agent.delete_session(session_id, *, base_url, api_key)` | Delete a session |

### `_AgentSession` (returned by `create` / `resume`)

| Method | Description |
|--------|-------------|
| `agent.send(message)` | Send a message, returns `Run` |
| `agent.close()` | Close and delete session (auto on context exit) |

### `Run` (returned by `send`)

| Method | Description |
|--------|-------------|
| `run.messages()` | Generator of typed messages (streaming) |
| `run.stream()` | Alias for `messages()` |
| `run.iter_text()` | Generator of text-only chunks |
| `run.text()` | Block and return all assistant text |
| `run.wait()` | Block until done, return `RunResult` |
| `run.supports(op)` | Check if operation is available |

### `RunResult`

| Field | Type | Description |
|-------|------|-------------|
| `id` | `str` | Session ID |
| `status` | `str` | `"finished"` \| `"error"` \| `"cancelled"` |
| `finish_reason` | `str \| None` | Provider finish reason |
| `usage` | `UsageMeta \| None` | Token usage |
| `error` | `str \| None` | Error message (when `status == "error"`) |
| `ok` | `bool` | Shorthand for `status == "finished"` |

### Message types

| Type | Description |
|------|-------------|
| `AssistantMessage` | LLM reply (has `.text()` helper, `.content: list`) |
| `UserMessage` | User or tool-result message |
| `SystemMessage` | System metadata (compaction boundaries, etc.) |

## Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `RECURSIVE_BASE_URL` | `http://127.0.0.1:3000` | Server URL |
| `RECURSIVE_API_KEY` | _(none)_ | API key for authenticated servers |
