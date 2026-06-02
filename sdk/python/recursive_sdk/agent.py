"""Agent — main entrypoint for the Recursive Agent SDK."""

from __future__ import annotations

import os
from typing import List, Optional

from ._http import _HttpClient
from .exceptions import RecursiveAgentError
from .models import GoalState, RunResult, SessionInfo
from .run import Run


class _AgentSession:
    """
    A persistent agent session.  Supports multi-turn conversations.

    Do not instantiate directly — use :meth:`Agent.create` or
    :meth:`Agent.resume`.

    Use as a context manager::

        with Agent.create(base_url="http://localhost:3000") as agent:
            run = agent.send("do something")
            run.wait()
    """

    def __init__(
        self,
        session_id: str,
        http: _HttpClient,
        *,
        _owns_session: bool = True,
    ) -> None:
        self._session_id = session_id
        self._http = http
        self._owns_session = _owns_session
        self._closed = False

    @property
    def session_id(self) -> str:
        """The underlying Recursive session ID."""
        return self._session_id

    # ── send ─────────────────────────────────────────────────────────────────

    def send(self, message: str) -> Run:
        """
        Send *message* to the agent and return a :class:`~recursive_sdk.run.Run`.

        The POST is dispatched in a background thread so the returned ``Run``
        can subscribe to SSE *before* the server starts emitting events —
        the broadcast channel does not replay missed events. Errors from the
        POST surface either through the SSE ``error`` event (HTTP 5xx,
        runtime failures) or through ``run.wait()`` returning the failure
        captured during dispatch.

        Example::

            run = agent.send("refactor src/main.rs")
            for msg in run.messages():
                if msg.type == "assistant":
                    print(msg.text(), end="")
            result = run.wait()
        """
        if self._closed:
            raise RecursiveAgentError("Agent session is already closed.")

        run = Run(session_id=self._session_id, http=self._http)

        def _dispatch() -> None:
            try:
                self._http.post(
                    f"/sessions/{self._session_id}/messages",
                    {"content": message},
                )
            except Exception as err:  # noqa: BLE001 — bubble via Run.wait()
                run._fail(str(err))

        import threading

        thread = threading.Thread(target=_dispatch, daemon=True)
        thread.start()
        run._send_thread = thread
        return run

    # ── Goal-168: goal loop ───────────────────────────────────────────────────

    def set_goal(
        self,
        condition: str,
        *,
        max_turns: int = 20,
    ) -> GoalState:
        """
        Start a condition-based autonomous loop for this session.

        The server evaluates *condition* after every agent turn and keeps
        looping until the condition is met or *max_turns* is exhausted.

        Returns a :class:`~recursive_sdk.models.GoalState` with
        ``status == "pursuing"``.

        Example::

            state = agent.set_goal("all tests pass", max_turns=10)
            print(state.status)  # "pursuing"
        """
        if self._closed:
            raise RecursiveAgentError("Agent session is already closed.")
        resp = self._http.post(
            f"/sessions/{self._session_id}/goal",
            {"condition": condition, "max_turns": max_turns},
        )
        data = resp.json()
        return GoalState(
            condition=condition,
            status=data.get("status", "pursuing"),
            turns=0,
            max_turns=max_turns,
        )

    def clear_goal(self) -> GoalState:
        """
        Clear the active goal for this session.

        Returns a :class:`~recursive_sdk.models.GoalState` with
        ``status == "cleared"``.
        """
        if self._closed:
            raise RecursiveAgentError("Agent session is already closed.")
        data = self._http.delete_json(f"/sessions/{self._session_id}/goal")
        return GoalState(
            condition="",
            status=data.get("status", "cleared"),
        )

    def get_goal(self) -> Optional[GoalState]:
        """
        Return the current goal state, or ``None`` if no goal is active.

        Calls ``GET /sessions/:id`` and extracts the ``goal`` field.
        """
        if self._closed:
            raise RecursiveAgentError("Agent session is already closed.")
        data = self._http.get(f"/sessions/{self._session_id}").json()
        raw = data.get("goal")
        if not raw:
            return None
        return GoalState(
            condition=raw.get("condition", ""),
            status=raw.get("status", "pursuing"),
            turns=raw.get("turns", 0),
            max_turns=raw.get("max_turns", 20),
            last_reason=raw.get("last_reason"),
        )

    # ── context manager ───────────────────────────────────────────────────────

    def close(self) -> None:
        """Close the session (deletes it on the server if we own it)."""
        if not self._closed:
            self._closed = True
            if self._owns_session:
                try:
                    self._http.delete(f"/sessions/{self._session_id}")
                except RecursiveAgentError:
                    pass  # best-effort
            self._http.close()

    def __enter__(self) -> "_AgentSession":
        return self

    def __exit__(self, *_: object) -> None:
        self.close()


class Agent:
    """
    Static factory for creating, resuming, and running agent sessions.

    Three invocation patterns:

    **One-shot** — send a prompt, get a result, done::

        result = Agent.prompt("list all TODO comments", base_url="http://localhost:3000")
        print(result.status, result.finish_reason)

    **Multi-turn** — create a session and send multiple messages::

        with Agent.create(base_url="http://localhost:3000") as agent:
            run = agent.send("Fix the test failures")
            result = run.wait()

            run2 = agent.send("Now update the docs")
            result2 = run2.wait()

    **Resume** — continue an existing session::

        with Agent.resume(session_id, base_url="http://localhost:3000") as agent:
            run = agent.send("Continue where we left off")
            run.wait()
    """

    # ── factory methods ───────────────────────────────────────────────────────

    @staticmethod
    def create(
        *,
        base_url: Optional[str] = None,
        api_key: Optional[str] = None,
        system_prompt: Optional[str] = None,
        timeout: float = 120.0,
    ) -> _AgentSession:
        """
        Create a new agent session.

        Parameters
        ----------
        base_url:
            URL of the Recursive server (default: ``RECURSIVE_BASE_URL`` env var
            or ``http://127.0.0.1:3000``).
        api_key:
            Optional API key (default: ``RECURSIVE_API_KEY`` env var).
        system_prompt:
            Optional system prompt for the session.
        timeout:
            HTTP / SSE timeout in seconds.
        """
        http = _make_client(base_url, api_key, timeout)
        body: dict = {}
        if system_prompt:
            body["system_prompt"] = system_prompt
        resp = http.post("/sessions", body)
        session_id = resp.json()["id"]
        return _AgentSession(session_id, http, _owns_session=True)

    @staticmethod
    def resume(
        session_id: str,
        *,
        base_url: Optional[str] = None,
        api_key: Optional[str] = None,
        timeout: float = 120.0,
    ) -> _AgentSession:
        """
        Resume an existing session by ID.

        The session is **not deleted** when the context manager exits (since we
        don't own it).
        """
        http = _make_client(base_url, api_key, timeout)
        # Verify the session exists
        http.get(f"/sessions/{session_id}")
        return _AgentSession(session_id, http, _owns_session=False)

    @staticmethod
    def prompt(
        message: str,
        *,
        base_url: Optional[str] = None,
        api_key: Optional[str] = None,
        system_prompt: Optional[str] = None,
        max_steps: Optional[int] = None,
        timeout: float = 120.0,
    ) -> RunResult:
        """
        One-shot convenience: create a session, send *message*, wait, delete.

        Returns a :class:`~recursive_sdk.models.RunResult`.

        Example::

            result = Agent.prompt("list files", base_url="http://localhost:3000")
            if result.status == "finished":
                print("done!")
        """
        http = _make_client(base_url, api_key, timeout)
        body: dict = {"goal": message}
        if system_prompt:
            body["system_prompt"] = system_prompt
        if max_steps is not None:
            body["max_steps"] = max_steps

        resp = http.post("/run", body)
        data = resp.json()
        usage = None
        if "usage" in data:
            from .run import _parse_usage

            usage = _parse_usage(data["usage"])
        return RunResult(
            id=data.get("session_id", ""),
            status=data.get("status", "finished"),
            finish_reason=data.get("finish_reason"),
            usage=usage,
            error=data.get("error"),
        )

    # ── session management helpers ────────────────────────────────────────────

    @staticmethod
    def list_sessions(
        *,
        base_url: Optional[str] = None,
        api_key: Optional[str] = None,
    ) -> List[SessionInfo]:
        """Return a list of active sessions."""
        http = _make_client(base_url, api_key)
        resp = http.get("/sessions")
        return [
            SessionInfo(
                id=s["id"],
                created_at=s.get("created_at", ""),
                message_count=s.get("message_count", 0),
                last_prompt=s.get("last_prompt"),
                first_prompt=s.get("first_prompt"),
                goal=s.get("goal"),
            )
            for s in resp.json()
        ]

    @staticmethod
    def delete_session(
        session_id: str,
        *,
        base_url: Optional[str] = None,
        api_key: Optional[str] = None,
    ) -> None:
        """Delete a session by ID."""
        http = _make_client(base_url, api_key)
        http.delete(f"/sessions/{session_id}")
        http.close()

    @staticmethod
    def get_session_messages(
        session_id: str,
        *,
        base_url: Optional[str] = None,
        api_key: Optional[str] = None,
    ) -> List[dict]:
        """
        Return the transcript messages for a session.

        Fetches ``GET /sessions/:id`` and returns the ``messages`` list.
        Each message is a raw dict with at minimum ``role`` and ``content`` keys.

        Example::

            msgs = Agent.get_session_messages(session_id)
            for m in msgs:
                print(m["role"], m["content"][:60])
        """
        http = _make_client(base_url, api_key)
        data = http.get(f"/sessions/{session_id}").json()
        http.close()
        return list(data.get("messages", []))


# ── helpers ───────────────────────────────────────────────────────────────


def _make_client(
    base_url: Optional[str],
    api_key: Optional[str],
    timeout: float = 120.0,
) -> _HttpClient:
    url = base_url or os.environ.get("RECURSIVE_BASE_URL", "http://127.0.0.1:3000")
    return _HttpClient(base_url=url, api_key=api_key, timeout=timeout)
