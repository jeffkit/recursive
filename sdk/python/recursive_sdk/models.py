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
class PartialAssistantMessage:
    """SDK Phase C: a streaming text delta from the assistant.

    Yielded by :meth:`Run.messages` as ``type="stream_event"`` events, one
    per token delta received from the LLM. Corresponds to
    ``SDKPartialAssistantMessage`` in the Claude Agent SDK.

    Callers that want token-level granularity (e.g. typewriter UI) can filter
    for ``msg.type == "stream_event"``. Most callers should use the full
    ``AssistantMessage`` (``type="assistant"``), which is emitted once the
    entire turn is complete and contains all text blocks.

    Example::

        for msg in run.messages():
            if msg.type == "stream_event":
                print(msg.text, end="", flush=True)
    """

    type: str  # "stream_event"
    text: str
    """The token delta text."""
    step: int = 0
    """Agent step index — use to group deltas from the same turn."""
    session_id: str = ""


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


def _map_finish_reason_to_subtype(finish_reason: Optional[str], status: str) -> str:
    """Map Rust FinishReason debug strings to Claude Agent SDK–compatible subtypes.

    Returns one of:
    - ``"success"``                — normal completion
    - ``"error_max_turns"``        — budget / turn limit exceeded
    - ``"error_during_execution"`` — runtime error (stuck, provider stop, etc.)
    - ``"cancelled"``              — interrupted / cancelled
    """
    if status == "cancelled":
        return "cancelled"
    if status != "finished" or finish_reason is None:
        if finish_reason and (
            "BudgetExceeded" in finish_reason or "TranscriptLimit" in finish_reason
        ):
            return "error_max_turns"
        return "error_during_execution"
    if "NoMoreToolCalls" in finish_reason or "PlanPending" in finish_reason:
        return "success"
    if "BudgetExceeded" in finish_reason or "TranscriptLimit" in finish_reason:
        return "error_max_turns"
    if "Cancelled" in finish_reason:
        return "cancelled"
    return "success"


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

    @property
    def subtype(self) -> str:
        """Claude Agent SDK–compatible result subtype.

        Maps the Rust ``finish_reason`` debug string to one of:
        ``"success"``, ``"error_max_turns"``, ``"error_during_execution"``,
        ``"cancelled"``.
        """
        return _map_finish_reason_to_subtype(self.finish_reason, self.status)


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
