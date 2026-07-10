"""Spawn ``recursive -p … --output-format stream-json`` and stream NDJSON."""

from __future__ import annotations

import os
import subprocess
from typing import Any, Dict, Generator, List, Optional

from .binary import find_recursive_binary
from .exceptions import RecursiveAgentError
from .wire import parse_wire_object


def build_cli_args(
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
) -> List[str]:
    """Build CLI argv for a one-shot / resume turn."""
    args = [
        "-p",
        prompt,
        "--output-format",
        "stream-json",
        "--permission-mode",
        _map_permission_mode(permission_mode, planning_mode),
        "-H",
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
    return args


def _map_permission_mode(
    mode: Optional[str],
    planning: Optional[str],
) -> str:
    if planning == "plan_first":
        return "plan"
    if mode in ("auto", "bypass"):
        return "auto"
    return "default"


class CliProcessHandle:
    """Handle for a running ``recursive`` CLI child process."""

    def __init__(
        self,
        proc: subprocess.Popen[str],
        resume_session_id: Optional[str] = None,
    ) -> None:
        self._proc = proc
        self._session_id = resume_session_id
        self._killed = False
        self._stderr_chunks: List[str] = []

    def cancel(self) -> None:
        if not self._killed and self._proc.poll() is None:
            self._killed = True
            self._proc.terminate()

    def get_session_id(self) -> Optional[str]:
        return self._session_id

    def items(self) -> Generator[Dict[str, Any], None, None]:
        assert self._proc.stdout is not None
        saw_result = False
        try:
            for line in self._proc.stdout:
                trimmed = line.strip()
                if not trimmed:
                    continue
                try:
                    import json

                    raw = json.loads(trimmed)
                except (json.JSONDecodeError, TypeError):
                    continue
                if not isinstance(raw, dict):
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
                yield item
        finally:
            if self._proc.stderr is not None:
                try:
                    err = self._proc.stderr.read()
                    if err:
                        self._stderr_chunks.append(err)
                except Exception:  # noqa: BLE001
                    pass
            self._proc.wait()

        if saw_result:
            return

        if self._killed:
            from .models import RunResult

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

        from .models import RunResult

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


def spawn_cli_process(
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
) -> CliProcessHandle:
    """Spawn the recursive CLI and return a handle that yields wire items."""
    bin_path = find_recursive_binary(cli_path)
    args = build_cli_args(
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
    )
    workdir = cwd or os.getcwd()
    try:
        proc = subprocess.Popen(
            [bin_path, *args],
            cwd=workdir,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=os.environ.copy(),
        )
    except OSError as err:
        raise RecursiveAgentError(
            f"failed to spawn recursive CLI ({bin_path}): {err}"
        ) from err

    return CliProcessHandle(proc, resume_session_id=resume_session_id)
