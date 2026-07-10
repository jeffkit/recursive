/**
 * Agent — main entrypoint for the Recursive Agent TypeScript SDK.
 *
 * Default transport: spawn the local `recursive` CLI with
 * `--output-format stream-json` (Claude Agent SDK–style).
 *
 * HTTP transport: pass `baseUrl` (or set `RECURSIVE_BASE_URL`) to talk to a
 * running `recursive http` server instead.
 */

import { RecursiveAgentError } from "./exceptions.js";
import { HttpClient } from "./http.js";
import { mapFinishReasonToSubtype } from "./models.js";
import type { RunResult, SessionInfo } from "./models.js";
import { Run } from "./run.js";
import { spawnCliProcess } from "./subprocess.js";

// ── AgentSession ──────────────────────────────────────────────────────────

/**
 * A persistent agent session. Supports multi-turn conversations.
 *
 * Do not instantiate directly — use `Agent.create()` or `Agent.resume()`.
 *
 * Use `await using` (TypeScript 5.2+) or a `try/finally` block for cleanup.
 */
export class AgentSession {
  private _sessionId: string;
  private readonly _http: HttpClient | null;
  private readonly _opts: AgentOptions;
  private readonly _ownsSession: boolean;
  private readonly _transport: "http" | "cli";
  private _closed = false;

  constructor(
    sessionId: string,
    http: HttpClient | null,
    options: {
      ownsSession: boolean;
      transport: "http" | "cli";
      agentOptions: AgentOptions;
    },
  ) {
    this._sessionId = sessionId;
    this._http = http;
    this._ownsSession = options.ownsSession;
    this._transport = options.transport;
    this._opts = options.agentOptions;
  }

  get sessionId(): string {
    return this._sessionId;
  }

  /**
   * Send *message* to the agent and return a `Run`.
   *
   * CLI transport: spawns `recursive -p …` (or `-r <id> -p …` for follow-ups).
   * HTTP transport: POSTs to `/sessions/:id/messages` and streams SSE.
   */
  async send(message: string): Promise<Run> {
    if (this._closed) {
      throw new RecursiveAgentError("Agent session is already closed.");
    }

    if (this._transport === "cli") {
      const resume =
        this._sessionId.length > 0 ? this._sessionId : undefined;
      const handle = spawnCliProcess({
        ...this._opts,
        prompt: message,
        resumeSessionId: resume,
        cwd: this._opts.cwd,
      });
      return Run._fromCli(this._sessionId, handle, (id) => {
        this._sessionId = id;
      });
    }

    if (!this._http) {
      throw new RecursiveAgentError("HTTP transport requires a client.");
    }

    const run = new Run(this._sessionId, this._http);
    const sendPromise = this._http
      .post(`/sessions/${this._sessionId}/messages`, { content: message })
      .catch((err) => {
        run._fail(err);
      });
    run._sendPromise = sendPromise;
    return run;
  }

  async close(): Promise<void> {
    if (!this._closed) {
      this._closed = true;
      if (this._ownsSession && this._http && this._sessionId) {
        try {
          await this._http.delete(`/sessions/${this._sessionId}`);
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
  /**
   * URL of a Recursive HTTP server.
   *
   * When set (or when `RECURSIVE_BASE_URL` is set), the SDK uses HTTP+SSE.
   * When omitted, the SDK spawns the local `recursive` CLI (default).
   */
  baseUrl?: string;
  /** API key for HTTP auth. Default: `RECURSIVE_API_KEY` env var. */
  apiKey?: string;
  /** HTTP timeout in milliseconds. Default: 120_000. */
  timeout?: number;
  /** Path to the `recursive` binary. Default: `RECURSIVE_BIN` or PATH. */
  cliPath?: string;
  /** Working directory / workspace for the CLI subprocess. Default: `process.cwd()`. */
  cwd?: string;
  /** Model id passed to the CLI (`-m`). */
  model?: string;
  /** Replace the default system prompt entirely. */
  systemPrompt?: string;
  /**
   * Append additional instructions to the default system prompt.
   * Ignored when `systemPrompt` is also provided.
   */
  appendSystemPrompt?: string;
  /** Human-readable display name for the session. */
  sessionName?: string;
  /** Maximum number of agent steps allowed. */
  maxSteps?: number;
  /**
   * Planning mode. `"immediate"` (default) executes tool calls right away;
   * `"plan_first"` buffers them and presents a plan for confirmation first.
   */
  planningMode?: "immediate" | "plan_first";
  /**
   * Extended-thinking token budget for models that support it.
   * Pass `0` to disable thinking. (HTTP transport only today.)
   */
  thinkingBudget?: number;
  /**
   * Permission mode. Controls tool-call enforcement:
   * - `"default"` — standard rules (default)
   * - `"auto"` — auto-approve (CLI: `--permission-mode auto`)
   * - `"strict"` — unknown tools are denied
   * - `"bypass"` — skip all permission rules (CLI maps to `auto`)
   */
  permissionMode?: "default" | "auto" | "strict" | "bypass";
  /** Maximum total API spend in USD for this session / run. */
  maxBudgetUsd?: number;
}

export interface PromptOptions extends AgentOptions {}

/**
 * Static factory for creating, resuming, and running agent sessions.
 *
 * ### Default (CLI subprocess)
 *
 * ```ts
 * const result = await Agent.prompt("List all TODO comments");
 * await using agent = await Agent.create();
 * const run = await agent.send("Fix the failing tests");
 * await run.wait();
 * ```
 *
 * ### HTTP (remote / shared server)
 *
 * ```ts
 * await using agent = await Agent.create({ baseUrl: "http://localhost:3000" });
 * ```
 */
export class Agent {
  /** Create a new agent session. */
  static async create(options: AgentOptions = {}): Promise<AgentSession> {
    if (usesHttp(options)) {
      const http = makeClient(options);
      const body: Record<string, unknown> = {};
      if (options.systemPrompt) body["system_prompt"] = options.systemPrompt;
      if (options.appendSystemPrompt)
        body["append_system_prompt"] = options.appendSystemPrompt;
      if (options.sessionName) body["session_name"] = options.sessionName;
      if (options.maxSteps != null) body["max_steps"] = options.maxSteps;
      if (options.planningMode) body["planning_mode"] = options.planningMode;
      if (options.thinkingBudget != null)
        body["thinking_budget"] = options.thinkingBudget;
      if (options.permissionMode)
        body["permission_mode"] = options.permissionMode;
      if (options.maxBudgetUsd != null)
        body["max_budget_usd"] = options.maxBudgetUsd;

      const data = (await http.post("/sessions", body)) as { id: string };
      return new AgentSession(data.id, http, {
        ownsSession: true,
        transport: "http",
        agentOptions: options,
      });
    }

    // CLI: session id is assigned on the first `send()` via system/init.
    return new AgentSession("", null, {
      ownsSession: true,
      transport: "cli",
      agentOptions: options,
    });
  }

  /**
   * Resume an existing session by ID.
   *
   * CLI: subsequent `send()` calls use `recursive -r <id> -p …`.
   * HTTP: verifies the session exists via GET.
   */
  static async resume(
    sessionId: string,
    options: AgentOptions = {},
  ): Promise<AgentSession> {
    if (usesHttp(options)) {
      const http = makeClient(options);
      await http.get(`/sessions/${sessionId}`);
      return new AgentSession(sessionId, http, {
        ownsSession: false,
        transport: "http",
        agentOptions: options,
      });
    }

    return new AgentSession(sessionId, null, {
      ownsSession: false,
      transport: "cli",
      agentOptions: options,
    });
  }

  /**
   * One-shot convenience: run *message* to completion and return `RunResult`.
   *
   * CLI (default): `recursive -p … --output-format stream-json`.
   * HTTP: `POST /run`.
   */
  static async prompt(
    message: string,
    options: PromptOptions = {},
  ): Promise<RunResult> {
    if (usesHttp(options)) {
      const http = makeClient(options);
      const body: Record<string, unknown> = { goal: message };
      if (options.systemPrompt) body["system_prompt"] = options.systemPrompt;
      if (options.appendSystemPrompt)
        body["append_system_prompt"] = options.appendSystemPrompt;
      if (options.maxSteps != null) body["max_steps"] = options.maxSteps;
      if (options.planningMode) body["planning_mode"] = options.planningMode;
      if (options.thinkingBudget != null)
        body["thinking_budget"] = options.thinkingBudget;
      if (options.permissionMode)
        body["permission_mode"] = options.permissionMode;
      if (options.maxBudgetUsd != null)
        body["max_budget_usd"] = options.maxBudgetUsd;

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

    const handle = spawnCliProcess({
      ...options,
      prompt: message,
      cwd: options.cwd,
    });
    const run = Run._fromCli("", handle);
    return run.wait();
  }

  /**
   * List active sessions (HTTP only).
   */
  static async listSessions(
    pagination: { limit?: number; offset?: number } = {},
    options: AgentOptions = {},
  ): Promise<SessionInfo[]> {
    requireHttp(options, "listSessions");
    const http = makeClient(options);
    const params = new URLSearchParams();
    if (pagination.limit != null) params.set("limit", String(pagination.limit));
    if (pagination.offset != null)
      params.set("offset", String(pagination.offset));
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

  /** Set a human-readable title for a session (HTTP only). */
  static async renameSession(
    sessionId: string,
    title: string,
    options: AgentOptions = {},
  ): Promise<void> {
    requireHttp(options, "renameSession");
    const http = makeClient(options);
    await http.patch(`/sessions/${sessionId}`, { title });
  }

  /** Return transcript messages for a session (HTTP only). */
  static async getSessionMessages(
    sessionId: string,
    options: AgentOptions = {},
  ): Promise<Record<string, unknown>[]> {
    requireHttp(options, "getSessionMessages");
    const http = makeClient(options);
    const data = (await http.get(`/sessions/${sessionId}`)) as Record<
      string,
      unknown
    >;
    return (data["messages"] as Record<string, unknown>[] | undefined) ?? [];
  }

  /** Fork a session (HTTP only). */
  static async forkSession(
    sessionId: string,
    options: AgentOptions = {},
  ): Promise<SessionInfo> {
    requireHttp(options, "forkSession");
    const http = makeClient(options);
    const data = (await http.post(`/sessions/${sessionId}/fork`, {})) as Record<
      string,
      unknown
    >;
    return {
      id: String(data["id"]),
      createdAt: String(data["created_at"] ?? ""),
      messageCount: Number(data["message_count"] ?? 0),
    };
  }

  /** Delete a session by ID (HTTP only). */
  static async deleteSession(
    sessionId: string,
    options: AgentOptions = {},
  ): Promise<void> {
    requireHttp(options, "deleteSession");
    const http = makeClient(options);
    await http.delete(`/sessions/${sessionId}`);
  }
}

// ── helpers ───────────────────────────────────────────────────────────────

function usesHttp(options: AgentOptions): boolean {
  return Boolean(options.baseUrl ?? process.env["RECURSIVE_BASE_URL"]);
}

function requireHttp(options: AgentOptions, method: string): void {
  if (!usesHttp(options)) {
    throw new RecursiveAgentError(
      `Agent.${method}() requires HTTP transport. Pass baseUrl or set RECURSIVE_BASE_URL.`,
    );
  }
}

function makeClient(options: AgentOptions): HttpClient {
  const baseUrl =
    options.baseUrl ??
    process.env["RECURSIVE_BASE_URL"] ??
    "http://127.0.0.1:3000";
  return new HttpClient({ baseUrl, apiKey: options.apiKey });
}
