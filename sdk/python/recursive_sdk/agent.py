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

        with Agent.create() as agent:  # CLI subprocess by default
            run = agent.send("do something")
            run.wait()
    """

    def __init__(
        self,
        session_id: str,
        http: Optional[_HttpClient],
        *,
        _owns_session: bool = True,
        _transport: str = "http",
        _opts: Optional[dict] = None,
    ) -> None:
        self._session_id = session_id
        self._http = http
        self._owns_session = _owns_session
        self._transport = _transport
        self._opts = _opts or {}
        self._closed = False

    @property
    def session_id(self) -> str:
        """The underlying Recursive session ID."""
        return self._session_id

    # ── send ─────────────────────────────────────────────────────────────────

    def send(self, message: str) -> Run:
        """
        Send *message* to the agent and return a :class:`~recursive_sdk.run.Run`.

        CLI transport: spawns ``recursive -p …`` (or ``-r <id> -p …``).
        HTTP transport: POSTs to ``/sessions/:id/messages`` and streams SSE.
        """
        if self._closed:
            raise RecursiveAgentError("Agent session is already closed.")

        if self._transport == "cli":
            from .cli import spawn_cli_process

            resume = self._session_id or None
            handle = spawn_cli_process(
                prompt=message,
                resume_session_id=resume,
                **{
                    k: v
                    for k, v in self._opts.items()
                    if k
                    in {
                        "cwd",
                        "cli_path",
                        "model",
                        "system_prompt",
                        "append_system_prompt",
                        "session_name",
                        "max_steps",
                        "max_budget_usd",
                        "planning_mode",
                        "permission_mode",
                    }
                },
            )

            def _on_sid(sid: str) -> None:
                self._session_id = sid

            return Run._from_cli(self._session_id, handle, _on_sid)

        if self._http is None:
            raise RecursiveAgentError("HTTP transport requires a client.")

        run = Run(session_id=self._session_id, http=self._http)

        def _dispatch() -> None:
            try:
                self._http.post(
                    f"/sessions/{self._session_id}/messages",
                    {"content": message},
                )
            except Exception as err:  # noqa: BLE001
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
        """Start a condition-based autonomous loop (HTTP only)."""
        self._require_http("set_goal")
        assert self._http is not None
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
        """Clear the active goal (HTTP only)."""
        self._require_http("clear_goal")
        assert self._http is not None
        if self._closed:
            raise RecursiveAgentError("Agent session is already closed.")
        data = self._http.delete_json(f"/sessions/{self._session_id}/goal")
        return GoalState(
            condition="",
            status=data.get("status", "cleared"),
        )

    def get_goal(self) -> Optional[GoalState]:
        """Return the current goal state (HTTP only)."""
        self._require_http("get_goal")
        assert self._http is not None
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

    def _require_http(self, method: str) -> None:
        if self._transport != "http" or self._http is None:
            raise RecursiveAgentError(
                f"{method}() requires HTTP transport. Pass base_url or set RECURSIVE_BASE_URL."
            )

    # ── context manager ───────────────────────────────────────────────────────

    def close(self) -> None:
        """Close the session (deletes it on the server if we own an HTTP session)."""
        if not self._closed:
            self._closed = True
            if self._owns_session and self._http and self._session_id:
                try:
                    self._http.delete(f"/sessions/{self._session_id}")
                except RecursiveAgentError:
                    pass
            if self._http is not None:
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
        append_system_prompt: Optional[str] = None,
        session_name: Optional[str] = None,
        max_steps: Optional[int] = None,
        planning_mode: Optional[str] = None,
        thinking_budget: Optional[int] = None,
        permission_mode: Optional[str] = None,
        max_budget_usd: Optional[float] = None,
        timeout: float = 120.0,
        cli_path: Optional[str] = None,
        cwd: Optional[str] = None,
        model: Optional[str] = None,
    ) -> _AgentSession:
        """
        Create a new agent session.

        Default transport is the local ``recursive`` CLI. Pass *base_url*
        (or set ``RECURSIVE_BASE_URL``) to use HTTP instead.
        """
        opts = {
            "system_prompt": system_prompt,
            "append_system_prompt": append_system_prompt,
            "session_name": session_name,
            "max_steps": max_steps,
            "planning_mode": planning_mode,
            "permission_mode": permission_mode,
            "max_budget_usd": max_budget_usd,
            "cli_path": cli_path,
            "cwd": cwd,
            "model": model,
        }
        if _uses_http(base_url):
            http = _make_client(base_url, api_key, timeout)
            body: dict = {}
            if system_prompt:
                body["system_prompt"] = system_prompt
            if append_system_prompt:
                body["append_system_prompt"] = append_system_prompt
            if session_name:
                body["session_name"] = session_name
            if max_steps is not None:
                body["max_steps"] = max_steps
            if planning_mode:
                body["planning_mode"] = planning_mode
            if thinking_budget is not None:
                body["thinking_budget"] = thinking_budget
            if permission_mode:
                body["permission_mode"] = permission_mode
            if max_budget_usd is not None:
                body["max_budget_usd"] = max_budget_usd
            resp = http.post("/sessions", body)
            session_id = resp.json()["id"]
            return _AgentSession(
                session_id,
                http,
                _owns_session=True,
                _transport="http",
                _opts=opts,
            )

        return _AgentSession(
            "",
            None,
            _owns_session=True,
            _transport="cli",
            _opts=opts,
        )

    @staticmethod
    def resume(
        session_id: str,
        *,
        base_url: Optional[str] = None,
        api_key: Optional[str] = None,
        timeout: float = 120.0,
        cli_path: Optional[str] = None,
        cwd: Optional[str] = None,
        model: Optional[str] = None,
        permission_mode: Optional[str] = None,
        max_steps: Optional[int] = None,
    ) -> _AgentSession:
        """Resume an existing session by ID."""
        opts = {
            "cli_path": cli_path,
            "cwd": cwd,
            "model": model,
            "permission_mode": permission_mode,
            "max_steps": max_steps,
        }
        if _uses_http(base_url):
            http = _make_client(base_url, api_key, timeout)
            http.get(f"/sessions/{session_id}")
            return _AgentSession(
                session_id,
                http,
                _owns_session=False,
                _transport="http",
                _opts=opts,
            )
        return _AgentSession(
            session_id,
            None,
            _owns_session=False,
            _transport="cli",
            _opts=opts,
        )

    @staticmethod
    def prompt(
        message: str,
        *,
        base_url: Optional[str] = None,
        api_key: Optional[str] = None,
        system_prompt: Optional[str] = None,
        append_system_prompt: Optional[str] = None,
        max_steps: Optional[int] = None,
        planning_mode: Optional[str] = None,
        thinking_budget: Optional[int] = None,
        permission_mode: Optional[str] = None,
        max_budget_usd: Optional[float] = None,
        timeout: float = 120.0,
        cli_path: Optional[str] = None,
        cwd: Optional[str] = None,
        model: Optional[str] = None,
    ) -> RunResult:
        """
        One-shot convenience: run *message* to completion.

        CLI (default): ``recursive -p … --output-format stream-json``.
        HTTP: ``POST /run``.
        """
        if _uses_http(base_url):
            http = _make_client(base_url, api_key, timeout)
            body: dict = {"goal": message}
            if system_prompt:
                body["system_prompt"] = system_prompt
            if append_system_prompt:
                body["append_system_prompt"] = append_system_prompt
            if max_steps is not None:
                body["max_steps"] = max_steps
            if planning_mode:
                body["planning_mode"] = planning_mode
            if thinking_budget is not None:
                body["thinking_budget"] = thinking_budget
            if permission_mode:
                body["permission_mode"] = permission_mode
            if max_budget_usd is not None:
                body["max_budget_usd"] = max_budget_usd

            resp = http.post("/run", body)
            data = resp.json()
            usage = None
            if "usage" in data:
                from .run import _parse_usage

                usage = _parse_usage(data["usage"])
            result = RunResult(
                id=data.get("session_id", ""),
                status=data.get("status", "finished"),
                finish_reason=data.get("finish_reason"),
                usage=usage,
                error=data.get("error"),
            )
            http.close()
            return result

        from .cli import spawn_cli_process

        handle = spawn_cli_process(
            prompt=message,
            cli_path=cli_path,
            cwd=cwd,
            model=model,
            system_prompt=system_prompt,
            append_system_prompt=append_system_prompt,
            max_steps=max_steps,
            max_budget_usd=max_budget_usd,
            planning_mode=planning_mode,
            permission_mode=permission_mode,
        )
        return Run._from_cli("", handle).wait()

    # ── session management helpers ────────────────────────────────────────────

    @staticmethod
    def list_sessions(
        *,
        limit: Optional[int] = None,
        offset: Optional[int] = None,
        base_url: Optional[str] = None,
        api_key: Optional[str] = None,
    ) -> List[SessionInfo]:
        """Return a list of active sessions.

        Args:
            limit: Maximum number of sessions to return (default: all).
            offset: Number of sessions to skip (default: 0).
        """
        http = _make_client(base_url, api_key)
        params: dict = {}
        if limit is not None:
            params["limit"] = limit
        if offset is not None:
            params["offset"] = offset
        url = "/sessions"
        if params:
            from urllib.parse import urlencode
            url = f"/sessions?{urlencode(params)}"
        resp = http.get(url)
        payload = resp.json()
        # Goal-293: the server now returns a `{ total, sessions }` envelope
        # so paginated UIs can render total counts without fetching every
        # page. Accept both the envelope and a bare list (older servers) so
        # the SDK is backward-compatible.
        if isinstance(payload, list):
            sessions_data = payload
        else:
            sessions_data = payload.get("sessions", [])
        return [
            SessionInfo(
                id=s["id"],
                created_at=s.get("created_at", ""),
                message_count=s.get("message_count", 0),
                last_prompt=s.get("last_prompt"),
                first_prompt=s.get("first_prompt"),
                goal=s.get("goal"),
            )
            for s in sessions_data
        ]

    @staticmethod
    def rename_session(
        session_id: str,
        title: str,
        *,
        base_url: Optional[str] = None,
        api_key: Optional[str] = None,
    ) -> None:
        """Set a human-readable title for a session.

        Calls ``PATCH /sessions/:id`` with ``{"title": title}``.
        Pass an empty string to clear the title.

        Args:
            session_id: Target session ID.
            title: New display title (pass ``""`` to clear).
        """
        http = _make_client(base_url, api_key)
        http.patch(f"/sessions/{session_id}", {"title": title})
        http.close()

    @staticmethod
    def fork_session(
        session_id: str,
        *,
        base_url: Optional[str] = None,
        api_key: Optional[str] = None,
    ) -> "SessionInfo":
        """Fork a session, copying its transcript to a new independent session.

        Calls ``POST /sessions/:id/fork`` and returns a
        :class:`~recursive_sdk.models.SessionInfo` for the newly created session.

        Example::

            forked = Agent.fork_session(session_id)
            print(forked.id, forked.message_count)
        """
        http = _make_client(base_url, api_key)
        resp = http.post(f"/sessions/{session_id}/fork", {})
        data = resp.json()
        http.close()
        return SessionInfo(
            id=data["id"],
            created_at=data.get("created_at", ""),
            message_count=data.get("message_count", 0),
        )

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


def _uses_http(base_url: Optional[str]) -> bool:
    return bool(base_url or os.environ.get("RECURSIVE_BASE_URL"))


def _make_client(
    base_url: Optional[str],
    api_key: Optional[str],
    timeout: float = 120.0,
) -> _HttpClient:
    url = base_url or os.environ.get("RECURSIVE_BASE_URL", "http://127.0.0.1:3000")
    return _HttpClient(base_url=url, api_key=api_key, timeout=timeout)
