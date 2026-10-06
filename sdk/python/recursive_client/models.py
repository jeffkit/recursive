"""Data models for Recursive Agent API responses."""

from dataclasses import dataclass, field, fields
from typing import Any, Dict, List, Literal, Optional


@dataclass
class ToolInfo:
    """A tool available to the agent."""

    name: str
    description: str
    parameters: Dict[str, Any]


@dataclass
class UsageInfo:
    """Token and step usage information.

    ``POST /run`` grew the cache split and the priced USD cost in issue #114,
    so every billing field beyond ``total_steps`` / ``total_tokens`` carries a
    default: a response from an older server still parses. Use
    :meth:`from_dict` rather than ``UsageInfo(**payload)`` — it drops keys the
    client does not know yet instead of raising, so a newer server cannot break
    an older client.
    """

    total_steps: int = 0
    total_tokens: int = 0
    prompt_tokens: int = 0
    completion_tokens: int = 0
    cache_hit_tokens: int = 0
    cache_miss_tokens: int = 0
    reasoning_tokens: int = 0
    llm_latency_ms: int = 0
    model: str = ""
    cost_usd: Optional[float] = None
    extra: Dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_dict(cls, payload: Dict[str, Any]) -> "UsageInfo":
        """Build from a server payload, keeping unknown keys in ``extra``."""
        known = {f.name for f in fields(cls)} - {"extra"}
        kwargs = {k: v for k, v in payload.items() if k in known}
        extra = {k: v for k, v in payload.items() if k not in known}
        return cls(extra=extra, **kwargs)


@dataclass
class RunResponse:
    """Response from a one-shot agent run."""

    status: str
    finish_reason: str
    messages: List[Any]
    usage: Any  # dict or UsageInfo

    def __post_init__(self):
        if isinstance(self.usage, dict):
            self.usage = UsageInfo.from_dict(self.usage)


@dataclass
class SessionInfo:
    """Summary info for a session."""

    id: str
    created_at: str
    message_count: int


@dataclass
class SessionList:
    """Response envelope for ``GET /sessions`` (Goal-293).

    Wraps the paginated list of :class:`SessionInfo` with a ``total`` count
    representing the **un-paginated** number of sessions known to the
    server, so clients can render "page X of Y" / scrollbars without
    fetching every page just to count sessions.

    ``limit`` and ``offset`` echo the pagination params back to the caller
    when available; they may be ``None`` if the caller didn't set them.
    """

    total: int
    sessions: List[SessionInfo]
    limit: Optional[int] = None
    offset: Optional[int] = None


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


# ── Goal-168: goal-loop models ──────────────────────────────────────────────

@dataclass
class GoalState:
    """Active goal loop state for a session."""

    condition: str
    status: str  # "pursuing" | "achieved" | "cleared"
    turns: int
    max_turns: int
    last_reason: Optional[str] = None


@dataclass
class SessionDetailWithGoal(SessionDetail):
    """Session detail including the active goal (Goal-168)."""

    goal: Optional[GoalState] = None

    def __post_init__(self):
        if isinstance(self.goal, dict):
            self.goal = GoalState(**self.goal)


# ── Goal-169: slash-command models ─────────────────────────────────────────

@dataclass
class SlashCommandInfo:
    """A registered slash command (built-in or skill-backed)."""

    name: str
    description: str
    source: str  # "builtin" | "skill"
    aliases: List[str] = field(default_factory=list)
    argument_hint: str = ""
