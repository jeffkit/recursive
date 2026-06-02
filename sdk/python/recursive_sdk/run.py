"""Run — represents a single agent turn in a session."""

from __future__ import annotations

from typing import Any, Dict, Generator, Iterator, Optional

from ._http import _HttpClient
from .models import (
    AssistantMessage,
    RunResult,
    SystemMessage,
    TextContent,
    ToolResultBlock,
    ToolUseBlock,
    UsageMeta,
    UserMessage,
)


class Run:
    """
    Represents a single agent turn (one ``agent.send(...)`` call).

    Usage::

        run = agent.send("Fix the bug")

        # Option A: stream tokens as they arrive
        for msg in run.messages():
            if msg.type == "assistant":
                print(msg.text(), end="", flush=True)

        # Option B: just wait for completion
        result = run.wait()
        print(result.status)

    ``messages()`` and ``stream()`` are aliases. ``wait()`` blocks until the
    run finishes and returns a :class:`RunResult`.
    """

    def __init__(self, session_id: str, http: _HttpClient) -> None:
        self._session_id = session_id
        self._http = http
        self._result: Optional[RunResult] = None
        # Reference to the dispatch thread set by `Agent.send()`. ``wait()``
        # joins this so callers don't need to track it themselves. Set
        # externally; ``None`` for runs constructed without a dispatcher
        # (e.g. tests).
        self._send_thread: Optional[Any] = None

    def _fail(self, err: str) -> None:
        """Record a dispatch failure. Surfaces through ``wait()``.

        Used by :meth:`Agent.send` to capture HTTP errors from the
        background POST so they propagate to the caller.
        """
        if self._result is None:
            self._result = RunResult(
                id=self._session_id,
                status="error",
                error=err,
            )

    @property
    def id(self) -> str:
        """Session ID for this run."""
        return self._session_id

    # ── streaming ────────────────────────────────────────────────────────────

    def messages(self) -> Generator[Any, None, None]:
        """
        Yield typed messages as they arrive from the server.

        Yields :class:`~recursive_sdk.models.AssistantMessage`,
        :class:`~recursive_sdk.models.UserMessage`, or
        :class:`~recursive_sdk.models.SystemMessage`.

        Calling ``messages()`` also populates ``self._result`` so that
        a subsequent ``wait()`` returns immediately without a second
        round-trip.
        """
        finish_reason: Optional[str] = None
        usage_data: Optional[Dict[str, Any]] = None
        run_status = "finished"

        for event in self._http.stream_events(
            f"/sessions/{self._session_id}/events"
        ):
            ev_type = event.get("type", "message")
            data = event.get("data", {})

            if ev_type in ("message", ""):
                msg = _parse_message(data, self._session_id)
                if msg is not None:
                    yield msg

            elif ev_type == "partial_message":
                # Streaming token deltas — surface as a SystemMessage so
                # callers that opt in via ``msg.subtype == 'partial_message'``
                # can render token-level UI. Default consumers will skip it.
                yield SystemMessage(
                    type="system",
                    subtype="partial_message",
                    data=data,
                )

            elif ev_type == "done":
                finish_reason = data.get("finish_reason")
                usage_data = data.get("usage")
                run_status = data.get("status", "finished")
                break

            elif ev_type == "error":
                run_status = "error"
                self._result = RunResult(
                    id=self._session_id,
                    status="error",
                    error=data.get("message", str(data)),
                )
                return

        usage = _parse_usage(usage_data) if usage_data else None
        self._result = RunResult(
            id=self._session_id,
            status=run_status,
            finish_reason=finish_reason,
            usage=usage,
        )

    # ``stream()`` is an alias for ``messages()``
    stream = messages

    def iter_text(self) -> Generator[str, None, None]:
        """Yield text chunks from assistant messages only."""
        for msg in self.messages():
            if isinstance(msg, AssistantMessage):
                for block in msg.content:
                    if isinstance(block, TextContent):
                        yield block.text

    def text(self) -> str:
        """Block until done, return all assistant text concatenated."""
        return "".join(self.iter_text())

    # ── wait ─────────────────────────────────────────────────────────────────

    def wait(self) -> RunResult:
        """
        Block until the run completes (drains the message stream if not already
        consumed) and return the terminal :class:`RunResult`.
        """
        if self._result is None:
            # Drain without exposing messages to the caller
            for _ in self.messages():
                pass
        # Make sure the dispatcher thread (if any) finished so a failed POST
        # has had a chance to call ``_fail()`` before we return.
        if self._send_thread is not None:
            try:
                self._send_thread.join(timeout=5.0)
            except Exception:
                pass
        assert self._result is not None
        return self._result

    # ── supports ─────────────────────────────────────────────────────────────

    def supports(self, operation: str) -> bool:
        """
        Check whether *operation* is supported for this run.
        Currently supported: ``"messages"``, ``"stream"``, ``"wait"``.
        """
        return operation in {"messages", "stream", "wait", "iter_text", "text"}


# ── helpers ───────────────────────────────────────────────────────────────


def _parse_message(data: Dict[str, Any], session_id: str) -> Any:
    role = data.get("role", "")
    content_raw = data.get("content", "")

    if role == "assistant":
        content = []
        if isinstance(content_raw, str):
            content = [TextContent(text=content_raw)]
        elif isinstance(content_raw, list):
            for item in content_raw:
                if isinstance(item, dict):
                    t = item.get("type", "")
                    if t == "text":
                        content.append(TextContent(text=item.get("text", "")))
                    elif t == "tool_use":
                        content.append(
                            ToolUseBlock(
                                id=item.get("id", ""),
                                name=item.get("name", ""),
                                input=item.get("input", {}),
                            )
                        )
        return AssistantMessage(type="assistant", content=content, session_id=session_id)

    elif role == "user":
        text = content_raw if isinstance(content_raw, str) else str(content_raw)
        return UserMessage(type="user", content=text, session_id=session_id)

    elif role == "system":
        return SystemMessage(
            type="system",
            subtype=data.get("subtype", ""),
            data=data,
        )

    return None


def _parse_usage(data: Dict[str, Any]) -> UsageMeta:
    return UsageMeta(
        input_tokens=data.get("input_tokens", 0),
        output_tokens=data.get("output_tokens", 0),
        cache_creation_tokens=data.get("cache_creation_tokens"),
        cache_read_tokens=data.get("cache_read_tokens"),
        reasoning_tokens=data.get("reasoning_tokens"),
    )
