"""Synchronous Python client for the Recursive Agent HTTP API."""

from typing import List, Optional

import requests

from .models import (
    MessageResponse,
    RunResponse,
    SessionDetail,
    SessionInfo,
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
        """Get session detail with messages."""
        resp = self.session.get(f"{self.base_url}/sessions/{session_id}")
        resp.raise_for_status()
        return SessionDetail(**resp.json())

    def delete_session(self, session_id: str) -> None:
        """Delete a session."""
        resp = self.session.delete(f"{self.base_url}/sessions/{session_id}")
        resp.raise_for_status()
