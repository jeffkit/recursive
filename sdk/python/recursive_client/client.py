"""Synchronous Python client for the Recursive Agent HTTP API."""

from typing import List, Optional

import requests

from .models import (
    GoalState,
    MessageResponse,
    PlanProposedMessage,
    RunResponse,
    SessionDetail,
    SessionInfo,
    SlashCommandInfo,
    ToolInfo,
)


class RecursiveClient:
    """Synchronous Python client for the Recursive Agent HTTP API."""

    def __init__(self, base_url: str = "http://127.0.0.1:3000"):
        self.base_url = base_url.rstrip("/")
        self.session = requests.Session()

    def health(self) -> str:
        """Check server health."""
        resp = self.session.get(f"{self.base_url}/health")
        resp.raise_for_status()
        return resp.text

    def list_tools(self) -> List[ToolInfo]:
        """List available tools."""
        resp = self.session.get(f"{self.base_url}/tools")
        resp.raise_for_status()
        return [ToolInfo(**t) for t in resp.json()]

    def run(
        self,
        goal: str,
        max_steps: Optional[int] = None,
        system_prompt: Optional[str] = None,
    ) -> RunResponse:
        """Execute agent with a goal (one-shot)."""
        body: dict = {"goal": goal}
        if max_steps is not None:
            body["max_steps"] = max_steps
        if system_prompt is not None:
            body["system_prompt"] = system_prompt
        resp = self.session.post(f"{self.base_url}/run", json=body)
        resp.raise_for_status()
        return RunResponse(**resp.json())

    def create_session(self, system_prompt: Optional[str] = None) -> str:
        """Create a session, return session ID."""
        body: dict = {}
        if system_prompt is not None:
            body["system_prompt"] = system_prompt
        resp = self.session.post(f"{self.base_url}/sessions", json=body)
        resp.raise_for_status()
        return resp.json()["id"]

    def list_sessions(self) -> List[SessionInfo]:
        """List active sessions."""
        resp = self.session.get(f"{self.base_url}/sessions")
        resp.raise_for_status()
        return [SessionInfo(**s) for s in resp.json()]

    def send_message(self, session_id: str, content: str) -> MessageResponse:
        """Send message to a session."""
        resp = self.session.post(
            f"{self.base_url}/sessions/{session_id}/messages",
            json={"content": content},
        )
        resp.raise_for_status()
        return MessageResponse(**resp.json())

    def get_session(self, session_id: str) -> SessionDetail:
        """Get session detail with messages (may include ``goal`` field)."""
        resp = self.session.get(f"{self.base_url}/sessions/{session_id}")
        resp.raise_for_status()
        data = resp.json()
        # Strip unknown fields so SessionDetail can be constructed safely.
        known = {k: v for k, v in data.items() if k in ("id", "created_at", "messages", "status", "pending_plan")}
        detail = SessionDetail(**known)
        # Attach raw goal data so callers can access it via detail.__dict__.
        raw_goal = data.get("goal")
        if raw_goal is not None:
            detail.__dict__["goal"] = raw_goal
        return detail

    def delete_session(self, session_id: str) -> None:
        """Delete a session."""
        resp = self.session.delete(f"{self.base_url}/sessions/{session_id}")
        resp.raise_for_status()

    def approve_plan(self, session_id: str, edits: Optional[str] = None) -> dict:
        """
        Approve the pending plan for a session in plan_pending_approval state.

        Args:
            session_id: The session ID.
            edits: Optional replacement plan text.

        Returns:
            {"status": "approved", "session_id": ...}
        """
        body: dict = {}
        if edits is not None:
            body["edits"] = edits
        resp = self.session.post(
            f"{self.base_url}/sessions/{session_id}/plan/confirm",
            json=body,
        )
        resp.raise_for_status()
        return resp.json()

    def reject_plan(self, session_id: str, reason: str = "") -> dict:
        """
        Reject the pending plan for a session in plan_pending_approval state.

        Args:
            session_id: The session ID.
            reason: Reason for rejection shown to the agent.

        Returns:
            {"status": "rejected", "session_id": ...}
        """
        resp = self.session.post(
            f"{self.base_url}/sessions/{session_id}/plan/reject",
            json={"reason": reason},
        )
        resp.raise_for_status()
        return resp.json()

    # ── Goal-168: goal-loop ─────────────────────────────────────────────────

    def set_goal(
        self,
        session_id: str,
        condition: str,
        max_turns: int = 20,
    ) -> dict:
        """
        Start a condition-based autonomous loop for a session.

        The server will run agent turns and evaluate the condition after each
        one until the condition is met or ``max_turns`` is exhausted.

        Args:
            session_id: The session ID.
            condition: Natural-language completion condition.
            max_turns: Hard cap on autonomous turns (default 20).

        Returns:
            ``{"status": "pursuing", "session_id": ...}``
        """
        resp = self.session.post(
            f"{self.base_url}/sessions/{session_id}/goal",
            json={"condition": condition, "max_turns": max_turns},
        )
        resp.raise_for_status()
        return resp.json()

    def clear_goal(self, session_id: str) -> dict:
        """
        Clear the active goal for a session.

        Args:
            session_id: The session ID.

        Returns:
            ``{"status": "cleared", "session_id": ...}``
        """
        resp = self.session.delete(f"{self.base_url}/sessions/{session_id}/goal")
        resp.raise_for_status()
        return resp.json()

    def get_goal(self, session_id: str) -> Optional[GoalState]:
        """
        Get the active goal for a session, or ``None`` if no goal is set.

        Args:
            session_id: The session ID.

        Returns:
            :class:`GoalState` or ``None``.
        """
        detail = self.get_session(session_id)
        raw = detail.__dict__.get("goal")
        if raw is None:
            return None
        if isinstance(raw, dict):
            return GoalState(**raw)
        return raw

    # ── Goal-169: slash commands ────────────────────────────────────────────

    def list_slash_commands(self) -> List[SlashCommandInfo]:
        """
        List all registered slash commands (built-in and skill-backed).

        Returns:
            List of :class:`SlashCommandInfo` objects.
        """
        resp = self.session.get(f"{self.base_url}/slash-commands")
        resp.raise_for_status()
        return [
            SlashCommandInfo(
                name=c["name"],
                description=c["description"],
                source=c["source"],
                aliases=c.get("aliases", []),
                argument_hint=c.get("argument_hint", ""),
            )
            for c in resp.json()
        ]
