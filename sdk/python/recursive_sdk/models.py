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


@dataclass
class ToolProgressMessage:
    """SDK Phase B: emitted when a tool call completes with timing info.

    Yielded by :meth:`Run.messages` as ``type="tool_progress"`` events.
    """

    type: str  # "tool_progress"
    tool_use_id: str
    """The tool call ID that just finished."""
    tool_name: str
    """Name of the tool that was called."""
    elapsed_ms: int
    """Wall-clock milliseconds from tool call start to result receipt."""
    session_id: str = ""


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
    result: Optional[str] = None
    """Concatenated final assistant text (collected while streaming)."""
    num_turns: int = 0
    """Number of assistant turns in this run."""
    duration_ms: Optional[int] = None
    """Wall-clock duration from first send to stream close, in milliseconds."""

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


# ── Goal-168: goal-loop ────────────────────────────────────────────────────

@dataclass
class GoalState:
    """
    Active goal-loop state for a session.

    Set via :meth:`~recursive_sdk.agent._AgentSession.set_goal`;
    cleared automatically when achieved or the turn budget is exhausted.
    """

    condition: str
    """Natural-language completion condition."""

    status: str
    """``"pursuing"`` | ``"achieved"`` | ``"cleared"``"""

    turns: int = 0
    """Turns taken so far in the loop."""

    max_turns: int = 20
    """Hard cap on autonomous turns before the loop stops."""

    last_reason: Optional[str] = None
    """Brief explanation from the last judge verdict."""
