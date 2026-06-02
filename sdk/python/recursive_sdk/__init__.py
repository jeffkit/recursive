"""
Recursive Agent SDK — Python client.

Quick start::

    from recursive_sdk import Agent

    # One-shot
    result = Agent.prompt("list files in the project", base_url="http://localhost:3000")
    print(result.status, result.finish_reason)

    # Multi-turn with streaming
    with Agent.create(base_url="http://localhost:3000") as agent:
        run = agent.send("Fix test failures in src/")
        for msg in run.messages():
            if msg.type == "assistant":
                print(msg.text(), end="", flush=True)
        result = run.wait()

    # Resume an existing session
    with Agent.resume(session_id, base_url="http://localhost:3000") as agent:
        run = agent.send("Continue from where you left off")
        run.wait()

Environment variables:

- ``RECURSIVE_BASE_URL`` — server URL (default: ``http://127.0.0.1:3000``)
- ``RECURSIVE_API_KEY``  — API key (if the server has auth enabled)
"""

from .agent import Agent
from .exceptions import RecursiveAgentError
from .models import (
    AssistantMessage,
    GoalState,
    RunResult,
    SessionInfo,
    SystemMessage,
    TextContent,
    ToolResultBlock,
    ToolUseBlock,
    UsageMeta,
    UserMessage,
)
from .run import Run

__version__ = "0.6.0"

__all__ = [
    # Main entrypoint
    "Agent",
    # Run
    "Run",
    "RunResult",
    # Message types
    "AssistantMessage",
    "UserMessage",
    "SystemMessage",
    "TextContent",
    "ToolUseBlock",
    "ToolResultBlock",
    # Goal-168
    "GoalState",
    # Misc
    "UsageMeta",
    "SessionInfo",
    "RecursiveAgentError",
]
