# recursive-sdk

Python SDK for the [Recursive Agent](https://github.com/recursive-agent/recursive).

Default transport spawns the local ``recursive`` CLI with Claude-compatible
``--output-format stream-json``.

## Install

```bash
pip install recursive-sdk
```

## Claude-compatible `query()` (recommended)

Same shape as ``claude_agent_sdk``:

```python
import asyncio
from recursive_sdk import query, ClaudeAgentOptions

async def main():
    async for message in query(
        prompt="Find and fix the bug in auth.py",
        options=ClaudeAgentOptions(
            max_turns=10,
            permission_mode="bypassPermissions",
            allowed_tools=["Read", "Write", "Bash"],
        ),
    ):
        if message.get("type") == "assistant":
            print(message["message"]["content"])
        if message.get("type") == "result":
            # Terminal result is IN the stream (Claude contract)
            print(message.get("subtype"), message.get("result"))

asyncio.run(main())
```

``query()`` opens the CLI **control channel** (no ``-H``): ``--output-format``
and ``--input-format stream-json``, so ``can_use_tool``, hooks, ``interrupt``,
and ``stream_input`` work like the Claude Agent SDK.

### Options (Claude names)

| Option | Maps to |
|--------|---------|
| ``cwd`` | ``--workspace`` |
| ``model`` | ``-m`` |
| ``max_turns`` | ``--max-steps`` |
| ``system_prompt`` | ``--system-prompt`` / append |
| ``permission_mode`` | ``--permission-mode`` (``bypassPermissions`` → ``auto``) |
| ``resume`` | ``-r`` |
| ``path_to_cli`` | binary path |
| ``max_budget_usd`` | ``--max-budget-usd`` |
| ``allowed_tools`` | ``--allow-tools`` |
| ``can_use_tool`` | control ``can_use_tool`` replies |
| ``hooks`` | ``initialize`` + ``hook_callback`` |

## Session-style API (also available)

```python
from recursive_sdk import Agent

result = Agent.prompt("List TODOs")
with Agent.create() as agent:
    agent.send("Fix tests").wait()
```

## Environment variables

| Variable | Description |
|----------|-------------|
| ``RECURSIVE_BIN`` | Path to the ``recursive`` binary |
| ``RECURSIVE_BASE_URL`` | When set, ``Agent.*`` uses HTTP |
| ``RECURSIVE_API_KEY`` | HTTP auth key |
