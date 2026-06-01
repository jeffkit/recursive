"""Public data models for the Recursive Agent SDK."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Dict, List, Optional


# ── Message types (yielded by Run.messages()) ─────────────────────────────

@dataclass
class TextContent:
    text: str
    type: str = "text"


@dataclass
class ToolUseBlock:
    id: str
    name: str
    input: Dict[str, Any]
    type: str = "tool_use"


@dataclass
class ToolResultBlock:
    tool_use_id: str
    content: str
    type: str = "tool_result"


@dataclass
class AssistantMessage:
    """An assistant reply message (streaming or final)."""
    type: str  # "assistant"
    content: List[Any]  # list of TextContent / ToolUseBlock
    session_id: str = ""

    def text(self) -> str:
        """Concatenate all text blocks."""
        parts = [b.text for b in self.content if isinstance(b, TextContent)]
        return "".join(parts)


@dataclass
class UserMessage:
    """A user (or tool-result) message."""
    type: str  # "user"
    content: str
    session_id: str = ""


@dataclass
class SystemMessage:
    """A system / metadata message (e.g. compact boundary)."""
    type: str  # "system"
    subtype: str = ""
    data: Dict[str, Any] = field(default_factory=dict)


# ── Run result ────────────────────────────────────────────────────────────

@dataclass
class UsageMeta:
    input_tokens: int = 0
    output_tokens: int = 0
    cache_creation_tokens: Optional[int] = None
    cache_read_tokens: Optional[int] = None
    reasoning_tokens: Optional[int] = None


@dataclass
class RunResult:
    """
    Terminal result of an agent run (returned by ``Run.wait()``).

    ``status`` is one of:
    - ``"finished"`` — agent completed normally
    - ``"error"``    — agent ran but encountered an error
    - ``"cancelled"`` — run was cancelled
    """

    id: str
    status: str
    finish_reason: Optional[str] = None
    usage: Optional[UsageMeta] = None
    error: Optional[str] = None

    @property
    def ok(self) -> bool:
        return self.status == "finished"


# ── Session info ──────────────────────────────────────────────────────────

@dataclass
class SessionInfo:
    id: str
    created_at: str
    message_count: int
    last_prompt: Optional[str] = None
    first_prompt: Optional[str] = None
    goal: Optional[str] = None
