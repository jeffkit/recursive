"""
Recursive Agent SDK — Python client.

Claude Agent SDK–compatible ``query()`` (recommended)::

    import asyncio
    from recursive_sdk import query, ClaudeAgentOptions

    async def main():
        async for message in query(
            prompt="list files in the project",
            options=ClaudeAgentOptions(max_turns=5),
        ):
            if message.get("type") == "result":
                print(message.get("result"))

    asyncio.run(main())

Session-style API (also available)::

    from recursive_sdk import Agent
    result = Agent.prompt("list files in the project")

Environment variables:

- ``RECURSIVE_BIN`` — path to the ``recursive`` binary (CLI transport)
- ``RECURSIVE_BASE_URL`` — when set, ``Agent.*`` uses HTTP instead of CLI
- ``RECURSIVE_API_KEY`` — API key (if the HTTP server has auth enabled)
"""

from .agent import Agent
from .exceptions import RecursiveAgentError
from .models import (
    AssistantMessage,
    GoalState,
    PartialAssistantMessage,
    RunResult,
    SessionInfo,
    SystemMessage,
    TextContent,
    ToolProgressMessage,
    ToolResultBlock,
    ToolUseBlock,
    UsageMeta,
    UserMessage,
)
from .query import ClaudeAgentOptions, Query, query
from .run import Run

__version__ = "0.6.0"

__all__ = [
    "query",
    "Query",
    "ClaudeAgentOptions",
    "Agent",
    "Run",
    "RunResult",
    "AssistantMessage",
    "UserMessage",
    "SystemMessage",
    "TextContent",
    "ToolUseBlock",
    "ToolResultBlock",
    "ToolProgressMessage",
    "PartialAssistantMessage",
    "GoalState",
    "UsageMeta",
    "SessionInfo",
    "RecursiveAgentError",
]
