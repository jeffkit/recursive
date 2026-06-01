"""Recursive Agent Python SDK — thin HTTP client."""

from .client import RecursiveClient
from .models import (
    GoalState,
    PlanProposedMessage,
    RunResponse,
    SessionDetail,
    SessionInfo,
    SlashCommandInfo,
    ToolInfo,
)

__version__ = "0.1.0"
__all__ = [
    "RecursiveClient",
    "GoalState",
    "PlanProposedMessage",
    "RunResponse",
    "SessionDetail",
    "SessionInfo",
    "SlashCommandInfo",
    "ToolInfo",
]
