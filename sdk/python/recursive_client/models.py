"""Data models for Recursive Agent API responses."""

from dataclasses import dataclass, field
from typing import Any, Dict, List, Literal, Optional


@dataclass
class ToolInfo:
    """A tool available to the agent."""

    name: str
    description: str
    parameters: Dict[str, Any]


@dataclass
class UsageInfo:
    """Token and step usage information."""

    total_steps: int
    total_tokens: int


@dataclass
class RunResponse:
    """Response from a one-shot agent run."""

    status: str
    finish_reason: str
    messages: List[Any]
    usage: Any  # dict or UsageInfo

    def __post_init__(self):
        if isinstance(self.usage, dict):
            self.usage = UsageInfo(**self.usage)


@dataclass
class SessionInfo:
    """Summary info for a session."""

    id: str
    created_at: str
    message_count: int


@dataclass
class SessionDetail:
    """Full session detail with messages."""

    id: str
    created_at: str
    messages: List[Any]
    status: str = "idle"
    pending_plan: Optional[str] = None


@dataclass
class MessageResponse:
    """Response from sending a message in a session."""

    role: str
    content: str


@dataclass
class PlanProposedMessage:
    """Emitted when the agent enters plan mode and proposes a plan."""

    plan: str = ""
    session_id: str = ""
    type: Literal["plan_proposed"] = "plan_proposed"
