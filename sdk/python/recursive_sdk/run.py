"""Run — represents a single agent turn in a session."""

from __future__ import annotations

import time
from typing import Any, Dict, Generator, List, Optional

from ._http import _HttpClient
from .models import (
    AssistantMessage,
    PartialAssistantMessage,
    RunResult,
    SystemMessage,
    TextContent,
    ToolProgressMessage,
    ToolUseBlock,
    UsageMeta,
    UserMessage,
)


class Run:
    """
    Represents a single agent turn (one ``agent.send(...)`` call).

    Backed by either HTTP SSE or a local ``recursive`` CLI subprocess.
    """

    def __init__(
        self,
        session_id: str,
        http: Optional[_HttpClient] = None,
        *,
        _cli_handle: Any = None,
        _on_session_id: Any = None,
    ) -> None:
        self._session_id = session_id
        self._http = http
        self._cli_handle = _cli_handle
        self._on_session_id = _on_session_id
        self._result: Optional[RunResult] = None
        self._send_thread: Optional[Any] = None

    @classmethod
    def _from_cli(
        cls,
        session_id: str,
        handle: Any,
        on_session_id: Any = None,
    ) -> "Run":
        return cls(
            session_id,
            http=None,
            _cli_handle=handle,
            _on_session_id=on_session_id,
        )

    def _fail(self, err: str) -> None:
        """Record a dispatch failure. Surfaces through ``wait()``."""
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
        """Yield typed messages as they arrive from the transport."""
        if self._cli_handle is not None:
            yield from self._messages_cli()
            return
        yield from self._messages_http()

    def _messages_cli(self) -> Generator[Any, None, None]:
        saw_result = False
        for item in self._cli_handle.items():
            if item["kind"] == "message":
                msg = item["message"]
                if (
                    isinstance(msg, SystemMessage)
                    and msg.subtype == "init"
                    and isinstance(msg.data.get("session_id"), str)
                ):
                    self._session_id = str(msg.data["session_id"])
                    if self._on_session_id is not None:
                        self._on_session_id(self._session_id)
                yield msg
            elif item["kind"] == "result":
                saw_result = True
                result: RunResult = item["result"]
                sid = self._cli_handle.get_session_id()
                if sid:
                    self._session_id = sid
                    if self._on_session_id is not None:
                        self._on_session_id(sid)
                    result.id = sid
                self._result = result
        if not saw_result and self._result is None:
            self._result = RunResult(
                id=self._session_id,
                status="error",
                error="CLI stream ended without a result",
            )

    def _messages_http(self) -> Generator[Any, None, None]:
        assert self._http is not None
        finish_reason: Optional[str] = None
        usage_data: Optional[Dict[str, Any]] = None
        run_status = "finished"
        result_parts: List[str] = []
        num_turns = 0
        start_ms = int(time.time() * 1000)

        for event in self._http.stream_events(
            f"/sessions/{self._session_id}/events"
        ):
            ev_type = event.get("type", "message")
            data = event.get("data", {})

            if ev_type in ("message", ""):
                msg = _parse_message(data, self._session_id)
                if msg is not None:
                    if isinstance(msg, AssistantMessage):
                        num_turns += 1
                        result_parts.append(msg.text())
                    yield msg

            elif ev_type == "partial_message":
                yield PartialAssistantMessage(
                    type="stream_event",
                    text=data.get("text", ""),
                    step=int(data.get("step", 0)),
                    session_id=self._session_id,
                )

            elif ev_type == "tool_progress":
                yield ToolProgressMessage(
                    type="tool_progress",
                    tool_use_id=data.get("tool_use_id", ""),
                    tool_name=data.get("tool_name", ""),
                    elapsed_ms=int(data.get("elapsed_ms", 0)),
                    session_id=self._session_id,
                )

            elif ev_type == "done":
                finish_reason = data.get("finish_reason")
                usage_data = data.get("usage")
                run_status = data.get("status", "finished")
                break

            elif ev_type == "error":
                self._result = RunResult(
                    id=self._session_id,
                    status="error",
                    error=data.get("message", str(data)),
                    num_turns=num_turns,
                    duration_ms=int(time.time() * 1000) - start_ms,
                )
                return

        duration_ms = int(time.time() * 1000) - start_ms
        usage = _parse_usage(usage_data) if usage_data else None
        self._result = RunResult(
            id=self._session_id,
            status=run_status,
            finish_reason=finish_reason,
            usage=usage,
            result="".join(result_parts) or None,
            num_turns=num_turns,
            duration_ms=duration_ms,
        )

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

    def wait(self) -> RunResult:
        """Block until the run completes and return the terminal RunResult."""
        if self._result is None:
            for _ in self.messages():
                pass
        if self._send_thread is not None:
            try:
                self._send_thread.join(timeout=5.0)
            except Exception:
                pass
        assert self._result is not None
        return self._result

    def cancel(self) -> None:
        """Request cancellation of the current run."""
        if self._cli_handle is not None:
            self._cli_handle.cancel()
            return
        if self._http is None:
            return
        try:
            self._http.post(
                f"/sessions/{self._session_id}/interrupt",
                {},
            )
        except Exception:
            pass

    def supports(self, operation: str) -> bool:
        return operation in {
            "messages",
            "stream",
            "wait",
            "iter_text",
            "text",
            "cancel",
        }


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
        return AssistantMessage(
            type="assistant", content=content, session_id=session_id
        )

    if role == "user":
        text = content_raw if isinstance(content_raw, str) else str(content_raw)
        return UserMessage(type="user", content=text, session_id=session_id)

    if role == "system":
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
