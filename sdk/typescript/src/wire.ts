/**
 * Parse Claude Code–compatible stream-json NDJSON into SDK message types.
 */

import { mapFinishReasonToSubtype } from "./models.js";
import type {
  AssistantMessage,
  ContentBlock,
  PartialAssistantMessage,
  RunResult,
  RunSubtype,
  SDKMessage,
  SystemMessage,
  UsageMeta,
  UserMessage,
} from "./models.js";

/** A parsed stream-json line: either a yieldable message or a terminal result. */
export type WireItem =
  | { kind: "message"; message: SDKMessage }
  | { kind: "result"; result: RunResult }
  | { kind: "session"; sessionId: string; message?: SDKMessage };

/**
 * Parse one NDJSON object from `recursive --output-format stream-json`.
 *
 * Returns `null` for lines that should be ignored (malformed / unknown).
 */
export function parseWireObject(
  raw: Record<string, unknown>,
  fallbackSessionId: string,
): WireItem | null {
  const type = String(raw["type"] ?? "");
  const sessionId = String(raw["session_id"] ?? fallbackSessionId);

  if (type === "system") {
    const subtype = String(raw["subtype"] ?? "");
    const msg: SystemMessage = {
      type: "system",
      subtype,
      data: raw,
    };
    if (subtype === "init" && raw["session_id"]) {
      return {
        kind: "session",
        sessionId: String(raw["session_id"]),
        message: msg,
      };
    }
    return { kind: "message", message: msg };
  }

  if (type === "assistant") {
    const message = raw["message"] as Record<string, unknown> | undefined;
    const content = parseContentBlocks(message?.["content"]);
    const msg: AssistantMessage = {
      type: "assistant",
      content,
      sessionId,
    };
    return { kind: "message", message: msg };
  }

  if (type === "user") {
    const message = raw["message"] as Record<string, unknown> | undefined;
    const contentRaw = message?.["content"];
    let content: string;
    if (typeof contentRaw === "string") {
      content = contentRaw;
    } else if (Array.isArray(contentRaw)) {
      content = JSON.stringify(contentRaw);
    } else {
      content = String(contentRaw ?? "");
    }
    const msg: UserMessage = {
      type: "user",
      content,
      sessionId,
    };
    return { kind: "message", message: msg };
  }

  if (type === "stream_event") {
    const event = raw["event"] as Record<string, unknown> | undefined;
    const delta = event?.["delta"] as Record<string, unknown> | undefined;
    const text = String(delta?.["text"] ?? "");
    if (!text) return null;
    const msg: PartialAssistantMessage = {
      type: "stream_event",
      text,
      step: Number(event?.["index"] ?? 0),
      sessionId,
    };
    return { kind: "message", message: msg };
  }

  if (type === "result") {
    return { kind: "result", result: parseResultObject(raw, sessionId) };
  }

  return null;
}

function parseContentBlocks(contentRaw: unknown): ContentBlock[] {
  const content: ContentBlock[] = [];
  if (typeof contentRaw === "string") {
    content.push({ type: "text", text: contentRaw });
    return content;
  }
  if (!Array.isArray(contentRaw)) return content;
  for (const item of contentRaw as Record<string, unknown>[]) {
    const t = item["type"] as string;
    if (t === "text") {
      content.push({ type: "text", text: String(item["text"] ?? "") });
    } else if (t === "tool_use") {
      content.push({
        type: "tool_use",
        id: String(item["id"] ?? ""),
        name: String(item["name"] ?? ""),
        input: (item["input"] as Record<string, unknown>) ?? {},
      });
    } else if (t === "tool_result") {
      content.push({
        type: "tool_result",
        tool_use_id: String(item["tool_use_id"] ?? ""),
        content: String(item["content"] ?? ""),
      });
    }
  }
  return content;
}

function parseResultObject(
  raw: Record<string, unknown>,
  sessionId: string,
): RunResult {
  const subtypeRaw = String(raw["subtype"] ?? "success");
  const isError = Boolean(raw["is_error"]);
  const subtype = normalizeSubtype(subtypeRaw);

  let status: RunResult["status"] = "finished";
  if (subtype === "cancelled") status = "cancelled";
  else if (isError || subtype !== "success") status = "error";

  const usageRaw = raw["usage"] as Record<string, unknown> | undefined;
  const usage: UsageMeta | undefined = usageRaw
    ? {
        inputTokens: Number(usageRaw["input_tokens"] ?? 0),
        outputTokens: Number(usageRaw["output_tokens"] ?? 0),
        cacheCreationTokens:
          usageRaw["cache_creation_input_tokens"] != null
            ? Number(usageRaw["cache_creation_input_tokens"])
            : undefined,
        cacheReadTokens:
          usageRaw["cache_read_input_tokens"] != null
            ? Number(usageRaw["cache_read_input_tokens"])
            : undefined,
      }
    : undefined;

  const errors = raw["errors"] as string[] | undefined;
  const error =
    errors && errors.length > 0
      ? errors.join("; ")
      : isError
        ? subtypeRaw
        : undefined;

  return {
    id: sessionId,
    status,
    subtype:
      subtype === "success" ||
      subtype === "error_max_turns" ||
      subtype === "error_during_execution" ||
      subtype === "cancelled"
        ? subtype
        : mapFinishReasonToSubtype(
            String(raw["stop_reason"] ?? ""),
            status,
          ),
    finishReason: raw["stop_reason"] != null ? String(raw["stop_reason"]) : undefined,
    usage,
    error,
    ok: status === "finished",
    result: raw["result"] != null ? String(raw["result"]) : undefined,
    numTurns:
      raw["num_turns"] != null ? Number(raw["num_turns"]) : undefined,
    durationMs:
      raw["duration_ms"] != null ? Number(raw["duration_ms"]) : undefined,
  };
}

function normalizeSubtype(raw: string): RunSubtype | string {
  if (
    raw === "success" ||
    raw === "error_max_turns" ||
    raw === "error_during_execution" ||
    raw === "cancelled" ||
    raw === "error_max_budget_usd"
  ) {
    if (raw === "error_max_budget_usd") return "error_during_execution";
    return raw;
  }
  return raw;
}
