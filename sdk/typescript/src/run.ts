/**
 * Run — represents a single agent turn in a session.
 */

import type { HttpClient } from "./http.js";
import type {
  AssistantMessage,
  ContentBlock,
  RunResult,
  SDKMessage,
  ToolProgressMessage,
  UsageMeta,
} from "./models.js";

export type { SDKMessage };

export class Run {
  /**
   * Session ID for this run.
   */
  readonly id: string;

  private readonly _http: HttpClient;
  private _result: RunResult | null = null;

  /**
   * Pending send POST issued by `Agent.send()`. Set by the Agent factory
   * after construction; awaited inside `wait()` so callers don't have to
   * separately await the dispatch.
   * @internal
   */
  _sendPromise: Promise<unknown> | null = null;

  constructor(sessionId: string, http: HttpClient) {
    this.id = sessionId;
    this._http = http;
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
        id: this.id,
        status: "error",
        error: msg,
        ok: false,
      };
    }
  }

  // ── streaming ────────────────────────────────────────────────────────────

  /**
   * Async iterable of typed messages as they arrive from the server.
   *
   * Drains the SSE stream and caches the terminal `RunResult` so that
   * a subsequent `wait()` returns immediately.
   *
   * ```ts
   * for await (const msg of run.stream()) {
   *   if (msg.type === "assistant") {
   *     for (const block of msg.content) {
   *       if (block.type === "text") process.stdout.write(block.text);
   *     }
   *   }
   * }
   * ```
   */
  async *stream(): AsyncGenerator<SDKMessage> {
    let finishReason: string | undefined;
    let usageData: Record<string, unknown> | undefined;
    let runStatus: "finished" | "error" | "cancelled" = "finished";
    const resultParts: string[] = [];
    let numTurns = 0;
    const startMs = Date.now();

    for await (const event of this._http.streamEvents(
      `/sessions/${this.id}/events`,
    )) {
      const evType = event.type;
      const data = event.data as Record<string, unknown>;

      if (evType === "message" || evType === "") {
        const msg = parseMessage(data, this.id);
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
        // Streaming token deltas — surface as a system message so callers
        // that want token-level granularity can opt in via msg.subtype.
        // Higher-level helpers (`iterText()`) ignore these by default since
        // the eventual `message` event will carry the full text.
        yield {
          type: "system",
          subtype: "partial_message",
          data,
        };
      } else if (evType === "tool_progress") {
        // SDK Phase B: tool execution timing event.
        const tp: ToolProgressMessage = {
          type: "tool_progress",
          toolUseId: String(data["tool_use_id"] ?? ""),
          toolName: String(data["tool_name"] ?? ""),
          elapsedMs: Number(data["elapsed_ms"] ?? 0),
          sessionId: this.id,
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
          id: this.id,
          status: "error",
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
      id: this.id,
      status: runStatus,
      finishReason,
      usage,
      ok: runStatus === "finished",
      result: resultParts.length > 0 ? resultParts.join("") : undefined,
      numTurns,
      durationMs: Date.now() - startMs,
    };
  }

  /**
   * Alias for `stream()` — matches the Claude Agent SDK naming.
   */
  messages(): AsyncGenerator<SDKMessage> {
    return this.stream();
  }

  /**
   * Async generator that yields only text chunks from assistant messages.
   */
  async *iterText(): AsyncGenerator<string> {
    for await (const msg of this.stream()) {
      if (msg.type === "assistant") {
        for (const block of msg.content) {
          if (block.type === "text") yield block.text;
        }
      }
    }
  }

  /**
   * Block until the run completes and return all assistant text concatenated.
   */
  async text(): Promise<string> {
    const chunks: string[] = [];
    for await (const chunk of this.iterText()) {
      chunks.push(chunk);
    }
    return chunks.join("");
  }

  // ── wait ─────────────────────────────────────────────────────────────────

  /**
   * Block until the run finishes (drains the stream if not already consumed)
   * and return the terminal `RunResult`.
   *
   * ```ts
   * const result = await run.wait();
   * if (result.status === "error") console.error(result.error);
   * ```
   */
  async wait(): Promise<RunResult> {
    if (this._result === null) {
      // Drain without exposing messages to the caller
      // eslint-disable-next-line @typescript-eslint/no-unused-vars
      for await (const _ of this.stream()) {
        // discard
      }
    }
    // Ensure any background POST has settled — surfaces _fail() results
    // even if the SSE stream never produced a terminal `done` event.
    if (this._sendPromise) {
      await this._sendPromise;
    }
    return this._result!;
  }

  // ── cancel ────────────────────────────────────────────────────────────────

  /**
   * Request cancellation of the current run.
   *
   * Sends `POST /sessions/:id/interrupt` to ask the server to stop the active
   * agent turn as soon as possible.  The stream will eventually close with
   * `status === "cancelled"`.  Best-effort — does not throw on network errors.
   */
  async cancel(): Promise<void> {
    try {
      await this._http.post(`/sessions/${this.id}/interrupt`, {});
    } catch {
      // best-effort; stream will eventually time out or close naturally
    }
  }

  // ── supports ─────────────────────────────────────────────────────────────

  /**
   * Check whether *operation* is supported for this run.
   */
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
      content: typeof contentRaw === "string" ? contentRaw : String(contentRaw ?? ""),
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
