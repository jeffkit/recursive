/**
 * Agent — main entrypoint for the Recursive Agent TypeScript SDK.
 */

import { RecursiveAgentError } from "./exceptions.js";
import { HttpClient } from "./http.js";
import { mapFinishReasonToSubtype } from "./models.js";
import type { RunResult, SessionInfo } from "./models.js";
import { Run } from "./run.js";

// ── AgentSession ──────────────────────────────────────────────────────────

/**
 * A persistent agent session. Supports multi-turn conversations.
 *
 * Do not instantiate directly — use `Agent.create()` or `Agent.resume()`.
 *
 * Use `await using` (TypeScript 5.2+) or a `try/finally` block for cleanup:
 *
 * ```ts
 * await using agent = await Agent.create({ baseUrl: "http://localhost:3000" });
 * const run = await agent.send("do something");
 * await run.wait();
 * ```
 */
export class AgentSession {
  readonly sessionId: string;

  private readonly _http: HttpClient;
  private readonly _ownsSession: boolean;
  private _closed = false;

  constructor(
    sessionId: string,
    http: HttpClient,
    options: { ownsSession: boolean },
  ) {
    this.sessionId = sessionId;
    this._http = http;
    this._ownsSession = options.ownsSession;
  }

  // ── send ─────────────────────────────────────────────────────────────────

  /**
   * Send *message* to the agent and return a `Run`.
   *
   * The POST is dispatched in the background — `send()` returns as soon
   * as the request is on the wire. The returned `Run` is lazy: the SSE
   * subscription opens when you iterate `run.stream()` or call
   * `run.wait()`. This avoids a race where the server-side run completes
   * (and broadcasts its events) before a subscriber attaches.
   *
   * ```ts
   * const run = await agent.send("Fix the failing tests");
   * for await (const msg of run.stream()) {
   *   if (msg.type === "assistant") {
   *     for (const block of msg.content) {
   *       if (block.type === "text") process.stdout.write(block.text);
   *     }
   *   }
   * }
   * const result = await run.wait();
   * ```
   */
  async send(message: string): Promise<Run> {
    if (this._closed) {
      throw new RecursiveAgentError("Agent session is already closed.");
    }
    // Fire-and-forget the POST so the SSE subscription in Run.stream()
    // can attach before the agent run starts emitting events. The server
    // creates the per-session broadcast channel inside the POST handler
    // before kicking off the runtime, so subscribers that connect within
    // the time it takes to issue this fetch will see every event.
    //
    // Errors from the POST surface either through the SSE `error` event
    // (HTTP 5xx, runtime failures) or through `wait()` returning the
    // failure stashed on the Run via `_fail()`.
    const run = new Run(this.sessionId, this._http);
    const sendPromise = this._http
      .post(`/sessions/${this.sessionId}/messages`, { content: message })
      .catch((err) => {
        run._fail(err);
      });
    run._sendPromise = sendPromise;
    return run;
  }

  // ── disposal ─────────────────────────────────────────────────────────────

  async close(): Promise<void> {
    if (!this._closed) {
      this._closed = true;
      if (this._ownsSession) {
        try {
          await this._http.delete(`/sessions/${this.sessionId}`);
        } catch {
          // best-effort
        }
      }
    }
  }

  /** `Symbol.asyncDispose` support — use with `await using`. */
  async [Symbol.asyncDispose](): Promise<void> {
    return this.close();
  }
}

// ── Agent (static factory) ────────────────────────────────────────────────

export interface AgentOptions {
  /** URL of the Recursive server. Default: `RECURSIVE_BASE_URL` env or `http://127.0.0.1:3000`. */
  baseUrl?: string;
  /** API key. Default: `RECURSIVE_API_KEY` env var. */
  apiKey?: string;
  /** HTTP timeout in milliseconds. Default: 120_000. */
  timeout?: number;
  /** System prompt for the session. */
  systemPrompt?: string;
}

export interface PromptOptions extends AgentOptions {
  maxSteps?: number;
}

/**
 * Static factory for creating, resuming, and running agent sessions.
 *
 * ### Three invocation patterns
 *
 * **One-shot** (`Agent.prompt`):
 * ```ts
 * const result = await Agent.prompt("List all TODO comments", {
 *   baseUrl: "http://localhost:3000",
 * });
 * console.log(result.status, result.finishReason);
 * ```
 *
 * **Multi-turn** (`Agent.create` + `agent.send`):
 * ```ts
 * await using agent = await Agent.create({ baseUrl: "http://localhost:3000" });
 * const run = await agent.send("Fix the test failures");
 * await run.wait();
 * const run2 = await agent.send("Update the docs");
 * await run2.wait();
 * ```
 *
 * **Resume** (`Agent.resume`):
 * ```ts
 * await using agent = await Agent.resume(sessionId, { baseUrl: "http://localhost:3000" });
 * const run = await agent.send("Continue where we left off");
 * await run.wait();
 * ```
 */
export class Agent {
  /** Create a new agent session. */
  static async create(options: AgentOptions = {}): Promise<AgentSession> {
    const http = makeClient(options);
    const body: Record<string, unknown> = {};
    if (options.systemPrompt) body["system_prompt"] = options.systemPrompt;

    const data = (await http.post("/sessions", body)) as { id: string };
    return new AgentSession(data.id, http, { ownsSession: true });
  }

  /**
   * Resume an existing session by ID.
   *
   * The session is **not deleted** on close (since we don't own it).
   */
  static async resume(
    sessionId: string,
    options: AgentOptions = {},
  ): Promise<AgentSession> {
    const http = makeClient(options);
    await http.get(`/sessions/${sessionId}`); // verify exists
    return new AgentSession(sessionId, http, { ownsSession: false });
  }

  /**
   * One-shot convenience: create a session, send *message*, wait, clean up.
   *
   * Returns a `RunResult`.
   */
  static async prompt(
    message: string,
    options: PromptOptions = {},
  ): Promise<RunResult> {
    const http = makeClient(options);
    const body: Record<string, unknown> = { goal: message };
    if (options.systemPrompt) body["system_prompt"] = options.systemPrompt;
    if (options.maxSteps != null) body["max_steps"] = options.maxSteps;

    const data = (await http.post("/run", body)) as Record<string, unknown>;
    const usageRaw = data["usage"] as Record<string, unknown> | undefined;

    const status = (data["status"] as RunResult["status"]) ?? "finished";
    const finishReason = data["finish_reason"] as string | undefined;
    return {
      id: String(data["session_id"] ?? ""),
      status,
      subtype: mapFinishReasonToSubtype(finishReason, status),
      finishReason,
      error: data["error"] as string | undefined,
      usage: usageRaw
        ? {
            inputTokens: Number(usageRaw["input_tokens"] ?? 0),
            outputTokens: Number(usageRaw["output_tokens"] ?? 0),
          }
        : undefined,
      ok: status === "finished",
    };
  }

  /**
   * List active sessions, with optional pagination.
   *
   * @param pagination - Optional `limit` and `offset` query params.
   * @param options - Connection options.
   */
  static async listSessions(
    pagination: { limit?: number; offset?: number } = {},
    options: AgentOptions = {},
  ): Promise<SessionInfo[]> {
    const http = makeClient(options);
    const params = new URLSearchParams();
    if (pagination.limit != null) params.set("limit", String(pagination.limit));
    if (pagination.offset != null) params.set("offset", String(pagination.offset));
    const url = params.size > 0 ? `/sessions?${params}` : "/sessions";
    const data = (await http.get(url)) as Array<Record<string, unknown>>;
    return data.map((s) => ({
      id: String(s["id"]),
      createdAt: String(s["created_at"] ?? ""),
      messageCount: Number(s["message_count"] ?? 0),
      lastPrompt: s["last_prompt"] as string | undefined,
      firstPrompt: s["first_prompt"] as string | undefined,
      goal: s["goal"] as string | undefined,
      title: s["title"] as string | undefined,
    }));
  }

  /**
   * Set a human-readable title for a session.
   *
   * Calls `PATCH /sessions/:id` with `{"title": title}`.
   * Pass an empty string to clear the title.
   */
  static async renameSession(
    sessionId: string,
    title: string,
    options: AgentOptions = {},
  ): Promise<void> {
    const http = makeClient(options);
    await http.patch(`/sessions/${sessionId}`, { title });
  }

  /** Delete a session by ID. */
  static async deleteSession(
    sessionId: string,
    options: AgentOptions = {},
  ): Promise<void> {
    const http = makeClient(options);
    await http.delete(`/sessions/${sessionId}`);
  }
}

// ── helpers ───────────────────────────────────────────────────────────────

function makeClient(options: AgentOptions): HttpClient {
  const baseUrl =
    options.baseUrl ??
    process.env["RECURSIVE_BASE_URL"] ??
    "http://127.0.0.1:3000";
  return new HttpClient({ baseUrl, apiKey: options.apiKey });
}
