"""Bidirectional Claude-compatible control session over ``recursive`` stdio."""

from __future__ import annotations

import json
import os
import subprocess
import threading
import uuid
from typing import Any, Callable, Dict, Generator, List, Optional

from .binary import find_recursive_binary
from .exceptions import RecursiveAgentError
from .wire import parse_wire_object

CanUseTool = Callable[..., Any]
HookCallback = Callable[..., Any]


def build_control_cli_args(
    *,
    prompt: str,
    resume_session_id: Optional[str] = None,
    cwd: Optional[str] = None,
    model: Optional[str] = None,
    system_prompt: Optional[str] = None,
    append_system_prompt: Optional[str] = None,
    session_name: Optional[str] = None,
    max_steps: Optional[int] = None,
    max_budget_usd: Optional[float] = None,
    planning_mode: Optional[str] = None,
    permission_mode: Optional[str] = None,
    allowed_tools: Optional[List[str]] = None,
) -> List[str]:
    """Build argv for a bidirectional control session (no ``-H``)."""
    args = [
        "-p",
        prompt,
        "--output-format",
        "stream-json",
        "--input-format",
        "stream-json",
        "--permission-mode",
        _map_permission_mode(permission_mode, planning_mode),
    ]
    if resume_session_id:
        args.extend(["-r", resume_session_id])
    if cwd:
        args.extend(["--workspace", cwd])
    if system_prompt:
        args.extend(["--system-prompt", system_prompt])
    if append_system_prompt:
        args.extend(["--append-system-prompt", append_system_prompt])
    if session_name:
        args.extend(["--name", session_name])
    if max_steps is not None:
        args.extend(["--max-steps", str(max_steps)])
    if max_budget_usd is not None:
        args.extend(["--max-budget-usd", str(max_budget_usd)])
    if model:
        args.extend(["-m", model])
    if allowed_tools:
        args.extend(["--allow-tools", ",".join(allowed_tools)])
    return args


def _map_permission_mode(
    mode: Optional[str],
    planning: Optional[str],
) -> str:
    if planning == "plan_first":
        return "plan"
    if mode in ("auto", "bypass"):
        return "auto"
    if mode == "strict":
        return "strict"
    return "default"


def _should_auto_allow(allowed_tools: Optional[List[str]], tool_name: str) -> bool:
    if not allowed_tools:
        return True
    return tool_name in allowed_tools


class ControlSessionHandle:
    """Handle for a running control-mode ``recursive`` child process."""

    def __init__(
        self,
        proc: subprocess.Popen[str],
        *,
        resume_session_id: Optional[str] = None,
        can_use_tool: Optional[CanUseTool] = None,
        allowed_tools: Optional[List[str]] = None,
        hook_callbacks: Optional[Dict[str, HookCallback]] = None,
        initialize_hooks: Optional[Dict[str, Any]] = None,
        keep_stdin_open: bool = False,
    ) -> None:
        self._proc = proc
        self._session_id = resume_session_id
        self._killed = False
        self._stdin_closed = False
        self._stderr_chunks: List[str] = []
        self._can_use_tool = can_use_tool
        self._allowed_tools = allowed_tools
        self._hook_callbacks = hook_callbacks or {}
        self._keep_stdin_open = keep_stdin_open
        self._pending: Dict[str, threading.Event] = {}
        self._pending_results: Dict[str, Dict[str, Any]] = {}
        self._write_lock = threading.Lock()

        if initialize_hooks:
            # Fire-and-forget: response is consumed later in items().
            request_id = str(uuid.uuid4())
            self._write_line(
                {
                    "type": "control_request",
                    "request_id": request_id,
                    "request": {
                        "subtype": "initialize",
                        "hooks": initialize_hooks,
                    },
                }
            )

    def cancel(self) -> None:
        if not self._killed and self._proc.poll() is None:
            self._killed = True
            self._proc.terminate()
        self.close()

    def close(self) -> None:
        if self._stdin_closed:
            return
        self._stdin_closed = True
        if self._proc.stdin is not None:
            try:
                self._proc.stdin.close()
            except Exception:  # noqa: BLE001
                pass

    def get_session_id(self) -> Optional[str]:
        return self._session_id

    def write_user(self, text: str) -> None:
        self._write_line(
            {"type": "user", "message": {"role": "user", "content": text}}
        )

    def interrupt(self) -> None:
        try:
            self._send_control_request({"subtype": "interrupt"})
        except Exception:  # noqa: BLE001
            pass
        self.cancel()

    def set_permission_mode(self, mode: str) -> None:
        self._send_control_request({"subtype": "set_permission_mode", "mode": mode})

    def set_model(self, model: str) -> None:
        self._send_control_request({"subtype": "set_model", "model": model})

    def _write_line(self, obj: Dict[str, Any]) -> None:
        if self._stdin_closed or self._proc.stdin is None:
            return
        with self._write_lock:
            try:
                self._proc.stdin.write(json.dumps(obj) + "\n")
                self._proc.stdin.flush()
            except Exception:  # noqa: BLE001
                pass

    def _send_control_request(self, request: Dict[str, Any]) -> Dict[str, Any]:
        request_id = str(uuid.uuid4())
        event = threading.Event()
        self._pending[request_id] = event
        self._write_line(
            {
                "type": "control_request",
                "request_id": request_id,
                "request": request,
            }
        )
        if not event.wait(timeout=30.0):
            self._pending.pop(request_id, None)
            raise TimeoutError("control_request timed out")
        return self._pending_results.pop(request_id, {})

    def _reply_control(self, request_id: str, response: Dict[str, Any]) -> None:
        self._write_line(
            {
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": request_id,
                    "response": response,
                },
            }
        )

    def _handle_control_request(
        self, request_id: str, request: Dict[str, Any]
    ) -> None:
        subtype = str(request.get("subtype") or "")
        if subtype == "can_use_tool":
            tool_name = str(request.get("tool_name") or "")
            tool_input = request.get("input") or {}
            tool_use_id = request.get("tool_use_id")
            if self._can_use_tool is not None:
                decision = self._can_use_tool(
                    tool_name,
                    tool_input,
                    {"tool_use_id": tool_use_id},
                )
                if not isinstance(decision, dict):
                    decision = {"behavior": "allow"}
            elif _should_auto_allow(self._allowed_tools, tool_name):
                decision = {"behavior": "allow"}
            else:
                decision = {
                    "behavior": "deny",
                    "message": (
                        f"tool '{tool_name}' not allowed "
                        "(pass can_use_tool or allowed_tools)"
                    ),
                }
            self._reply_control(request_id, decision)
            return

        if subtype == "hook_callback":
            callback_id = str(request.get("callback_id") or "")
            cb = self._hook_callbacks.get(callback_id)
            result: Dict[str, Any] = {}
            if cb is not None:
                out = cb(
                    request.get("input") or {},
                    request.get("tool_use_id"),
                    {},
                )
                if isinstance(out, dict):
                    result = out
            self._reply_control(request_id, result)
            return

        self._reply_control(request_id, {})

    def items(self) -> Generator[Dict[str, Any], None, None]:
        assert self._proc.stdout is not None
        saw_result = False
        try:
            for line in self._proc.stdout:
                trimmed = line.strip()
                if not trimmed:
                    continue
                try:
                    raw = json.loads(trimmed)
                except (json.JSONDecodeError, TypeError):
                    continue
                if not isinstance(raw, dict):
                    continue

                typ = str(raw.get("type") or "")
                if typ == "control_request":
                    request_id = str(raw.get("request_id") or "")
                    request = raw.get("request") or {}
                    if isinstance(request, dict):
                        self._handle_control_request(request_id, request)
                    continue
                if typ == "control_response":
                    response = raw.get("response") or {}
                    if isinstance(response, dict):
                        request_id = str(
                            response.get("request_id") or raw.get("request_id") or ""
                        )
                        event = self._pending.pop(request_id, None)
                        if event is not None:
                            body = response.get("response")
                            self._pending_results[request_id] = (
                                body if isinstance(body, dict) else response
                            )
                            event.set()
                    continue

                item = parse_wire_object(raw, self._session_id or "")
                if item is None:
                    continue
                if item["kind"] == "session":
                    self._session_id = item["session_id"]
                    if item.get("message") is not None:
                        yield {"kind": "message", "message": item["message"]}
                    continue
                msg = item.get("message")
                if (
                    item["kind"] == "message"
                    and getattr(msg, "type", None) == "assistant"
                ):
                    msg.session_id = self._session_id or msg.session_id
                if item["kind"] == "result":
                    saw_result = True
                    if not self._keep_stdin_open:
                        self.close()
                yield item
        finally:
            if self._proc.stderr is not None:
                try:
                    err = self._proc.stderr.read()
                    if err:
                        self._stderr_chunks.append(err)
                except Exception:  # noqa: BLE001
                    pass
            self.close()
            self._proc.wait()

        if saw_result:
            return

        from .models import RunResult

        if self._killed:
            yield {
                "kind": "result",
                "result": RunResult(
                    id=self._session_id or "",
                    status="cancelled",
                    finish_reason="Cancelled",
                    error="cancelled",
                ),
            }
            return

        err_tail = "".join(self._stderr_chunks).strip()[-500:]
        code = self._proc.returncode
        yield {
            "kind": "result",
            "result": RunResult(
                id=self._session_id or "",
                status="error",
                finish_reason="ProviderStop",
                error=err_tail
                or f"recursive CLI exited with code {code} without a result",
            ),
        }


def spawn_control_session(
    *,
    prompt: str,
    resume_session_id: Optional[str] = None,
    cwd: Optional[str] = None,
    cli_path: Optional[str] = None,
    model: Optional[str] = None,
    system_prompt: Optional[str] = None,
    append_system_prompt: Optional[str] = None,
    session_name: Optional[str] = None,
    max_steps: Optional[int] = None,
    max_budget_usd: Optional[float] = None,
    planning_mode: Optional[str] = None,
    permission_mode: Optional[str] = None,
    allowed_tools: Optional[List[str]] = None,
    can_use_tool: Optional[CanUseTool] = None,
    hook_callbacks: Optional[Dict[str, HookCallback]] = None,
    initialize_hooks: Optional[Dict[str, Any]] = None,
    keep_stdin_open: bool = False,
) -> ControlSessionHandle:
    """Spawn the recursive CLI in control mode."""
    bin_path = find_recursive_binary(cli_path)
    args = build_control_cli_args(
        prompt=prompt,
        resume_session_id=resume_session_id,
        cwd=cwd,
        model=model,
        system_prompt=system_prompt,
        append_system_prompt=append_system_prompt,
        session_name=session_name,
        max_steps=max_steps,
        max_budget_usd=max_budget_usd,
        planning_mode=planning_mode,
        permission_mode=permission_mode,
        allowed_tools=allowed_tools,
    )
    workdir = cwd or os.getcwd()
    try:
        proc = subprocess.Popen(
            [bin_path, *args],
            cwd=workdir,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
            env=os.environ.copy(),
        )
    except OSError as err:
        raise RecursiveAgentError(
            f"failed to spawn recursive CLI ({bin_path}): {err}"
        ) from err

    return ControlSessionHandle(
        proc,
        resume_session_id=resume_session_id,
        can_use_tool=can_use_tool,
        allowed_tools=allowed_tools,
        hook_callbacks=hook_callbacks,
        initialize_hooks=initialize_hooks,
        keep_stdin_open=keep_stdin_open,
    )
