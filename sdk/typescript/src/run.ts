/**
 * Run — represents a single agent turn in a session.
 *
 * Backed by either the HTTP SSE transport or a local `recursive` CLI
 * subprocess (`--output-format stream-json`).
 */

import type { HttpClient } from "./http.js";
import { mapFinishReasonToSubtype } from "./models.js";
import type {
  ContentBlock,
  PartialAssistantMessage,
  RunResult,
  SDKMessage,
  ToolProgressMessage,
  UsageMeta,
} from "./models.js";
import type { CliProcessHandle } from "./subprocess.js";

export type { SDKMessage };

type RunBackend =
  | { kind: "http"; http: HttpClient }
  | {
      kind: "cli";
      handle: CliProcessHandle;
      /** Called when `system/init` reveals the session id. */
      onSessionId?: (sessionId: string) => void;
    };

export class Run {
  private _id: string;
  private _backend: RunBackend;
  private _result: RunResult | null = null;

  /**
   * Pending send POST issued by HTTP `Agent.send()`. Set by the Agent
   * factory after construction; awaited inside `wait()`.
   * @internal
   */
  _sendPromise: Promise<unknown> | null = null;

  constructor(sessionId: string, http: HttpClient) {
    this._id = sessionId;
    this._backend = { kind: "http", http };
  }

  /** @internal Construct a CLI-backed run. */
  static _fromCli(
    sessionId: string,
    handle: CliProcessHandle,
    onSessionId?: (sessionId: string) => void,
  ): Run {
    const run = new Run(sessionId, /* unused */ undefined as unknown as HttpClient);
    run._backend = { kind: "cli", handle, onSessionId };
    run._result = null;
    run._sendPromise = null;
    return run;
  }

  /** Session ID for this run (may be empty until CLI `system/init`). */
  get id(): string {
    return this._id;
  }

  /**
   * Used by `Agent.send()` to record a failed POST. The error becomes
   * the `RunResult` when `wait()` is next called.
   * @internal
   */
  _fail(err: unknown): void {
    if (this._result === null) {
      const msg =
        err instanceof Error ? err.message : `failed to send message: ${err}`;
      this._result = {
        id: this._id,
        status: "error",
        subtype: "error_during_execution",
        error: msg,
        ok: false,
      };
    }
  }

  // ── streaming ────────────────────────────────────────────────────────────

  /**
   * Async iterable of typed messages as they arrive.
   *
   * Drains the underlying transport and caches the terminal `RunResult`
   * so a subsequent `wait()` returns immediately.
   */
  async *stream(): AsyncGenerator<SDKMessage> {
    if (this._backend.kind === "cli") {
      yield* this._streamCli();
      return;
    }
    yield* this._streamHttp();
  }

  private async *_streamCli(): AsyncGenerator<SDKMessage> {
    if (this._backend.kind !== "cli") return;
    const { handle, onSessionId } = this._backend;
    let sawResult = false;

    for await (const item of handle.items()) {
      if (item.kind === "message") {
        if (
          item.message.type === "system" &&
          item.message.subtype === "init" &&
          typeof item.message.data["session_id"] === "string"
        ) {
          this._id = String(item.message.data["session_id"]);
          onSessionId?.(this._id);
        }
        yield item.message;
      } else if (item.kind === "result") {
        sawResult = true;
        if (!item.result.id && this._id) item.result.id = this._id;
        this._result = item.result;
        // Capture session id from the handle if init was only seen as session.
        const sid = handle.getSessionId();
        if (sid) {
          this._id = sid;
          onSessionId?.(sid);
          this._result.id = sid;
        }
      }
    }

    if (!sawResult && this._result === null) {
      this._result = {
        id: this._id,
        status: "error",
        subtype: "error_during_execution",
        error: "CLI stream ended without a result",
        ok: false,
      };
    }
  }

  private async *_streamHttp(): AsyncGenerator<SDKMessage> {
    if (this._backend.kind !== "http") return;
    const http = this._backend.http;

    let finishReason: string | undefined;
    let usageData: Record<string, unknown> | undefined;
    let runStatus: "finished" | "error" | "cancelled" = "finished";
    const resultParts: string[] = [];
    let numTurns = 0;
    const startMs = Date.now();

    for await (const event of http.streamEvents(
      `/sessions/${this._id}/events`,
    )) {
      const evType = event.type;
      const data = event.data as Record<string, unknown>;

      if (evType === "message" || evType === "") {
        const msg = parseMessage(data, this._id);
        if (msg) {
          if (msg.type === "assistant") {
            numTurns++;
            for (const block of msg.content) {
              if (block.type === "text") resultParts.push(block.text);
            }
          }
          yield msg;
        }
      } else if (evType === "partial_message") {
        const pm: PartialAssistantMessage = {
          type: "stream_event",
          text: String(data["text"] ?? ""),
          step: Number(data["step"] ?? 0),
          sessionId: this._id,
        };
        yield pm;
      } else if (evType === "tool_progress") {
        const tp: ToolProgressMessage = {
          type: "tool_progress",
          toolUseId: String(data["tool_use_id"] ?? ""),
          toolName: String(data["tool_name"] ?? ""),
          elapsedMs: Number(data["elapsed_ms"] ?? 0),
          sessionId: this._id,
        };
        yield tp;
      } else if (evType === "done") {
        finishReason = data["finish_reason"] as string | undefined;
        usageData = data["usage"] as Record<string, unknown> | undefined;
        runStatus = (data["status"] as typeof runStatus | undefined) ?? "finished";
        break;
      } else if (evType === "error") {
        runStatus = "error";
        this._result = {
          id: this._id,
          status: "error",
          subtype: "error_during_execution",
          error: String(data["message"] ?? data),
          ok: false,
          numTurns,
          durationMs: Date.now() - startMs,
        };
        return;
      }
    }

    const usage = usageData ? parseUsage(usageData) : undefined;
    this._result = {
      id: this._id,
      status: runStatus,
      subtype: mapFinishReasonToSubtype(finishReason, runStatus),
      finishReason,
      usage,
      ok: runStatus === "finished",
      result: resultParts.length > 0 ? resultParts.join("") : undefined,
      numTurns,
      durationMs: Date.now() - startMs,
    };
  }

  /** Alias for `stream()` — matches the Claude Agent SDK naming. */
  messages(): AsyncGenerator<SDKMessage> {
    return this.stream();
  }

  /** Async generator that yields only text chunks from assistant messages. */
  async *iterText(): AsyncGenerator<string> {
    for await (const msg of this.stream()) {
      if (msg.type === "assistant") {
        for (const block of msg.content) {
          if (block.type === "text") yield block.text;
        }
      }
    }
  }

  /** Block until the run completes and return all assistant text concatenated. */
  async text(): Promise<string> {
    const chunks: string[] = [];
    for await (const chunk of this.iterText()) {
      chunks.push(chunk);
    }
    return chunks.join("");
  }

  // ── wait ─────────────────────────────────────────────────────────────────

  /**
   * Block until the run finishes and return the terminal `RunResult`.
   */
  async wait(): Promise<RunResult> {
    if (this._result === null) {
      for await (const _ of this.stream()) {
        // discard
      }
    }
    if (this._sendPromise) {
      await this._sendPromise;
    }
    return this._result!;
  }

  // ── cancel ────────────────────────────────────────────────────────────────

  /**
   * Request cancellation of the current run.
   *
   * HTTP: `POST /sessions/:id/interrupt`.
   * CLI: SIGTERM the child process.
   */
  async cancel(): Promise<void> {
    if (this._backend.kind === "cli") {
      this._backend.handle.cancel();
      return;
    }
    try {
      await this._backend.http.post(`/sessions/${this._id}/interrupt`, {});
    } catch {
      // best-effort
    }
  }

  /** Check whether *operation* is supported for this run. */
  supports(operation: string): boolean {
    return ["stream", "messages", "iterText", "text", "wait", "cancel"].includes(
      operation,
    );
  }
}

// ── helpers ───────────────────────────────────────────────────────────────

function parseMessage(
  data: Record<string, unknown>,
  sessionId: string,
): SDKMessage | null {
  const role = data["role"] as string | undefined;
  const contentRaw = data["content"];

  if (role === "assistant") {
    const content: ContentBlock[] = [];

    if (typeof contentRaw === "string") {
      content.push({ type: "text", text: contentRaw });
    } else if (Array.isArray(contentRaw)) {
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
        }
      }
    }

    return { type: "assistant", content, sessionId };
  }

  if (role === "user") {
    return {
      type: "user",
      content:
        typeof contentRaw === "string"
          ? contentRaw
          : String(contentRaw ?? ""),
      sessionId,
    };
  }

  if (role === "system" || (data["type"] as string) === "system") {
    return {
      type: "system",
      subtype: String(data["subtype"] ?? ""),
      data,
    };
  }

  return null;
}

function parseUsage(data: Record<string, unknown>): UsageMeta {
  return {
    inputTokens: Number(data["input_tokens"] ?? 0),
    outputTokens: Number(data["output_tokens"] ?? 0),
    cacheCreationTokens:
      data["cache_creation_tokens"] != null
        ? Number(data["cache_creation_tokens"])
        : undefined,
    cacheReadTokens:
      data["cache_read_tokens"] != null
        ? Number(data["cache_read_tokens"])
        : undefined,
    reasoningTokens:
      data["reasoning_tokens"] != null
        ? Number(data["reasoning_tokens"])
        : undefined,
  };
}
