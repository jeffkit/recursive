"""Parse Claude Code–compatible stream-json NDJSON into SDK message types."""

from __future__ import annotations

from typing import Any, Dict, List, Optional

from .models import (
    AssistantMessage,
    PartialAssistantMessage,
    RunResult,
    SystemMessage,
    TextContent,
    ToolResultBlock,
    ToolUseBlock,
    UsageMeta,
    UserMessage,
    _map_finish_reason_to_subtype,
)

WireItem = Dict[str, Any]  # {"kind": "message"|"result"|"session", ...}


def parse_wire_object(
    raw: Dict[str, Any],
    fallback_session_id: str = "",
) -> Optional[WireItem]:
    """Parse one NDJSON object from ``recursive --output-format stream-json``."""
    typ = str(raw.get("type", ""))
    session_id = str(raw.get("session_id") or fallback_session_id)

    if typ == "system":
        subtype = str(raw.get("subtype", ""))
        msg = SystemMessage(type="system", subtype=subtype, data=raw)
        if subtype == "init" and raw.get("session_id"):
            return {
                "kind": "session",
                "session_id": str(raw["session_id"]),
                "message": msg,
            }
        return {"kind": "message", "message": msg}

    if typ == "assistant":
        message = raw.get("message") or {}
        content = _parse_content_blocks(message.get("content"))
        return {
            "kind": "message",
            "message": AssistantMessage(
                type="assistant", content=content, session_id=session_id
            ),
        }

    if typ == "user":
        message = raw.get("message") or {}
        content_raw = message.get("content")
        if isinstance(content_raw, str):
            content = content_raw
        elif isinstance(content_raw, list):
            import json

            content = json.dumps(content_raw)
        else:
            content = str(content_raw or "")
        return {
            "kind": "message",
            "message": UserMessage(
                type="user", content=content, session_id=session_id
            ),
        }

    if typ == "stream_event":
        event = raw.get("event") or {}
        delta = event.get("delta") or {}
        text = str(delta.get("text") or "")
        if not text:
            return None
        return {
            "kind": "message",
            "message": PartialAssistantMessage(
                type="stream_event",
                text=text,
                step=int(event.get("index") or 0),
                session_id=session_id,
            ),
        }

    if typ == "result":
        return {
            "kind": "result",
            "result": _parse_result_object(raw, session_id),
        }

    return None


def _parse_content_blocks(content_raw: Any) -> List[Any]:
    content: List[Any] = []
    if isinstance(content_raw, str):
        content.append(TextContent(text=content_raw))
        return content
    if not isinstance(content_raw, list):
        return content
    for item in content_raw:
        if not isinstance(item, dict):
            continue
        t = item.get("type")
        if t == "text":
            content.append(TextContent(text=str(item.get("text") or "")))
        elif t == "tool_use":
            content.append(
                ToolUseBlock(
                    id=str(item.get("id") or ""),
                    name=str(item.get("name") or ""),
                    input=item.get("input") or {},
                )
            )
        elif t == "tool_result":
            content.append(
                ToolResultBlock(
                    tool_use_id=str(item.get("tool_use_id") or ""),
                    content=str(item.get("content") or ""),
                )
            )
    return content


def _parse_result_object(raw: Dict[str, Any], session_id: str) -> RunResult:
    subtype_raw = str(raw.get("subtype") or "success")
    is_error = bool(raw.get("is_error"))

    if subtype_raw == "error_max_budget_usd":
        subtype = "error_during_execution"
    elif subtype_raw in (
        "success",
        "error_max_turns",
        "error_during_execution",
        "cancelled",
    ):
        subtype = subtype_raw
    else:
        subtype = _map_finish_reason_to_subtype(
            str(raw.get("stop_reason") or ""),
            "error" if is_error else "finished",
        )

    status = "finished"
    if subtype == "cancelled":
        status = "cancelled"
    elif is_error or subtype != "success":
        status = "error"

    usage = None
    usage_raw = raw.get("usage")
    if isinstance(usage_raw, dict):
        usage = UsageMeta(
            input_tokens=int(usage_raw.get("input_tokens") or 0),
            output_tokens=int(usage_raw.get("output_tokens") or 0),
            cache_creation_tokens=(
                int(usage_raw["cache_creation_input_tokens"])
                if usage_raw.get("cache_creation_input_tokens") is not None
                else None
            ),
            cache_read_tokens=(
                int(usage_raw["cache_read_input_tokens"])
                if usage_raw.get("cache_read_input_tokens") is not None
                else None
            ),
        )

    errors = raw.get("errors")
    error = None
    if isinstance(errors, list) and errors:
        error = "; ".join(str(e) for e in errors)
    elif is_error:
        error = subtype_raw

    return RunResult(
        id=session_id,
        status=status,
        finish_reason=_finish_reason_for_subtype(subtype, raw.get("stop_reason")),
        usage=usage,
        error=error,
        result=str(raw["result"]) if raw.get("result") is not None else None,
        num_turns=int(raw["num_turns"]) if raw.get("num_turns") is not None else 0,
        duration_ms=(
            int(raw["duration_ms"]) if raw.get("duration_ms") is not None else None
        ),
    )


def _finish_reason_for_subtype(
    subtype: str,
    stop_reason: Any,
) -> Optional[str]:
    """Pick a finish_reason that makes ``RunResult.subtype`` match the wire label."""
    if stop_reason is not None:
        return str(stop_reason)
    if subtype == "success":
        return "NoMoreToolCalls"
    if subtype == "error_max_turns":
        return "BudgetExceeded"
    if subtype == "cancelled":
        return "Cancelled"
    return subtype
