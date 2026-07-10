"""Claude Agent SDK–compatible ``query()`` entrypoint.

Example::

    import asyncio
    from recursive_sdk import query, ClaudeAgentOptions

    async def main():
        async for message in query(
            prompt="Find and fix the bug in auth.py",
            options=ClaudeAgentOptions(
                max_turns=10,
                permission_mode="bypassPermissions",
            ),
        ):
            if message.get("type") == "result":
                print(message.get("result"))

    asyncio.run(main())
"""

from __future__ import annotations

import asyncio
import threading
from dataclasses import dataclass, field
from typing import Any, AsyncIterator, Callable, Dict, List, Optional, Union

from .control_session import spawn_control_session

CanUseTool = Callable[..., Any]
HookCallback = Callable[..., Any]


@dataclass
class ClaudeAgentOptions:
    """Subset of Claude Agent SDK options that Recursive can honour today."""

    cwd: Optional[str] = None
    model: Optional[str] = None
    max_turns: Optional[int] = None
    system_prompt: Optional[Union[str, Dict[str, Any]]] = None
    permission_mode: Optional[str] = None
    resume: Optional[str] = None
    path_to_cli: Optional[str] = None
    """Path to the ``recursive`` binary (Claude: path_to_claude_code_executable)."""
    max_budget_usd: Optional[float] = None
    allowed_tools: List[str] = field(default_factory=list)
    can_use_tool: Optional[CanUseTool] = None
    hooks: Optional[Dict[str, Any]] = None
    # Accepted for call-site portability; mid-run MCP apply is CLI best-effort.
    mcp_servers: Dict[str, Any] = field(default_factory=dict)


class Query:
    """Async iterator with Claude-style control methods (sync constructor)."""

    def __init__(self, handle: Any) -> None:
        self._handle = handle
        self._queue: Optional[asyncio.Queue[Optional[Dict[str, Any]]]] = None
        self._thread: Optional[threading.Thread] = None
        self._started = False

    def _ensure_started(self) -> asyncio.Queue[Optional[Dict[str, Any]]]:
        if self._started and self._queue is not None:
            return self._queue
        loop = asyncio.get_running_loop()
        queue: asyncio.Queue[Optional[Dict[str, Any]]] = asyncio.Queue()
        handle = self._handle

        def _worker() -> None:
            try:
                for item in handle.items():
                    msg = _wire_item_to_query_message(item)
                    if msg is not None:
                        loop.call_soon_threadsafe(queue.put_nowait, msg)
            except Exception as err:  # noqa: BLE001
                loop.call_soon_threadsafe(
                    queue.put_nowait,
                    {
                        "type": "result",
                        "subtype": "error_during_execution",
                        "is_error": True,
                        "errors": [str(err)],
                    },
                )
            finally:
                loop.call_soon_threadsafe(queue.put_nowait, None)

        self._thread = threading.Thread(target=_worker, daemon=True)
        self._thread.start()
        self._queue = queue
        self._started = True
        return queue

    def __aiter__(self) -> "Query":
        return self

    async def __anext__(self) -> Dict[str, Any]:
        queue = self._ensure_started()
        try:
            msg = await queue.get()
        except (asyncio.CancelledError, GeneratorExit):
            self._handle.cancel()
            raise
        if msg is None:
            if self._thread is not None:
                self._thread.join(timeout=5.0)
            raise StopAsyncIteration
        return msg

    async def interrupt(self) -> None:
        await asyncio.to_thread(self._handle.interrupt)

    def close(self) -> None:
        self._handle.close()
        self._handle.cancel()

    async def stream_input(self, prompts: AsyncIterator[str]) -> None:
        async for text in prompts:
            await asyncio.to_thread(self._handle.write_user, text)
        await asyncio.to_thread(self._handle.close)

    async def set_permission_mode(self, mode: str) -> None:
        await asyncio.to_thread(self._handle.set_permission_mode, mode)

    async def set_model(self, model: str) -> None:
        await asyncio.to_thread(self._handle.set_model, model)


def query(
    *,
    prompt: str,
    options: Optional[ClaudeAgentOptions] = None,
) -> Query:
    """
    Run an agent turn and yield Claude-compatible message dicts.

    The terminal ``type: "result"`` object is yielded **inside** the stream
    (same contract as ``claude_agent_sdk.query``).
    """
    opts = options or ClaudeAgentOptions()
    hook_callbacks, initialize_hooks = _materialize_hooks(opts.hooks)
    spawn_kwargs = _options_to_spawn(prompt, opts)
    spawn_kwargs["hook_callbacks"] = hook_callbacks
    spawn_kwargs["initialize_hooks"] = initialize_hooks or None
    handle = spawn_control_session(**spawn_kwargs)
    return Query(handle)


def _materialize_hooks(
    hooks: Optional[Dict[str, Any]],
) -> tuple[Dict[str, HookCallback], Dict[str, Any]]:
    callbacks: Dict[str, HookCallback] = {}
    initialize: Dict[str, Any] = {}
    if not hooks:
        return callbacks, initialize
    n = 0
    for event, matchers in hooks.items():
        ids: List[str] = []
        if not isinstance(matchers, list):
            continue
        for matcher in matchers:
            if not isinstance(matcher, dict):
                continue
            for cb in matcher.get("hooks") or []:
                cb_id = f"hook_{event}_{n}"
                n += 1
                callbacks[cb_id] = cb
                ids.append(cb_id)
        if ids:
            initialize[event] = [{"hookCallbackIds": ids}]
    return callbacks, initialize


def _options_to_spawn(prompt: str, opts: ClaudeAgentOptions) -> Dict[str, Any]:
    system_prompt: Optional[str] = None
    append_system_prompt: Optional[str] = None
    if isinstance(opts.system_prompt, str):
        system_prompt = opts.system_prompt
    elif isinstance(opts.system_prompt, dict):
        append_system_prompt = opts.system_prompt.get("append")

    permission_mode = _map_permission(opts.permission_mode)
    planning_mode = None
    if opts.permission_mode == "plan":
        planning_mode = "plan_first"
        permission_mode = "default"

    return {
        "prompt": prompt,
        "cwd": opts.cwd,
        "model": opts.model,
        "max_steps": opts.max_turns,
        "max_budget_usd": opts.max_budget_usd,
        "system_prompt": system_prompt,
        "append_system_prompt": append_system_prompt,
        "resume_session_id": opts.resume,
        "cli_path": opts.path_to_cli,
        "permission_mode": permission_mode,
        "planning_mode": planning_mode,
        "allowed_tools": opts.allowed_tools or None,
        "can_use_tool": opts.can_use_tool,
    }


def _map_permission(mode: Optional[str]) -> Optional[str]:
    if mode in (
        "bypassPermissions",
        "acceptEdits",
        "dontAsk",
        "auto",
        "bypass",
    ):
        return "auto"
    if mode == "strict":
        return "strict"
    if mode in ("default", "plan", None):
        return "default"
    return "default"


def _wire_item_to_query_message(item: Dict[str, Any]) -> Optional[Dict[str, Any]]:
    kind = item.get("kind")
    if kind == "result":
        r = item["result"]
        msg: Dict[str, Any] = {
            "type": "result",
            "subtype": r.subtype,
            "is_error": not r.ok,
            "session_id": r.id,
            "num_turns": r.num_turns,
            "duration_ms": r.duration_ms,
            "stop_reason": r.finish_reason,
        }
        if r.result is not None:
            msg["result"] = r.result
        if r.usage is not None:
            msg["usage"] = {
                "input_tokens": r.usage.input_tokens,
                "output_tokens": r.usage.output_tokens,
                "cache_read_input_tokens": r.usage.cache_read_tokens,
                "cache_creation_input_tokens": r.usage.cache_creation_tokens,
            }
        if r.error:
            msg["errors"] = [r.error]
        return msg

    if kind != "message":
        return None

    m = item["message"]
    mtype = getattr(m, "type", None)
    if mtype == "assistant":
        content = []
        for b in m.content:
            if getattr(b, "type", None) == "text":
                content.append({"type": "text", "text": b.text})
            else:
                content.append(
                    {
                        "type": "tool_use",
                        "id": getattr(b, "id", ""),
                        "name": getattr(b, "name", ""),
                        "input": getattr(b, "input", {}),
                    }
                )
        return {
            "type": "assistant",
            "session_id": m.session_id,
            "parent_tool_use_id": None,
            "message": {"role": "assistant", "content": content},
        }
    if mtype == "user":
        return {
            "type": "user",
            "session_id": m.session_id,
            "parent_tool_use_id": None,
            "message": {"role": "user", "content": m.content},
        }
    if mtype == "system":
        out = {"type": "system", "subtype": m.subtype, **m.data}
        if "session_id" in m.data:
            out["session_id"] = m.data["session_id"]
        return out
    if mtype == "stream_event":
        return {
            "type": "stream_event",
            "session_id": m.session_id,
            "parent_tool_use_id": None,
            "event": {
                "type": "content_block_delta",
                "index": m.step,
                "delta": {"type": "text_delta", "text": m.text},
            },
        }
    if mtype == "tool_progress":
        return {
            "type": "tool_progress",
            "session_id": m.session_id,
            "tool_use_id": m.tool_use_id,
            "tool_name": m.tool_name,
            "elapsed_time_seconds": m.elapsed_ms / 1000.0,
        }
    return None


# Exported for unit tests
options_to_spawn = _options_to_spawn
map_permission = _map_permission
wire_item_to_query_message = _wire_item_to_query_message
