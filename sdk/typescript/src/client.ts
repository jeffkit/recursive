/**
 * RecursiveClient — low-level HTTP client for the Recursive Agent.
 *
 * Mirrors `recursive_client.client.RecursiveClient` from the Python SDK and
 * gives you direct access to every endpoint exposed by the Rust server,
 * including Plan Mode 2.0 (g165–167), the autonomous goal loop (g168), and
 * skill-backed slash commands (g169).
 *
 * For the high-level multi-turn ergonomics use {@link Agent} instead.
 *
 * ```ts
 * import { RecursiveClient } from "@recursive/sdk";
 *
 * const client = new RecursiveClient({ baseUrl: "http://localhost:3000" });
 *
 * // Plan Mode 2.0 — approve a pending plan after review.
 * await client.approvePlan(sessionId, { edits: "tweaked plan…" });
 *
 * // Goal-168 — drive an autonomous loop until the condition is met.
 * await client.setGoal(sessionId, "all tests pass", { maxTurns: 30 });
 * const goal = await client.getGoal(sessionId);
 * if (goal?.status === "achieved") await client.clearGoal(sessionId);
 *
 * // Goal-169 — discover registered slash commands.
 * const cmds = await client.listSlashCommands();
 * ```
 */

import { HttpClient } from "./http.js";
import type {
  GoalActionResponse,
  GoalState,
  PlanApprovalResponse,
  SessionDetail,
  SessionInfo,
  SlashCommandInfo,
  ToolInfo,
} from "./models.js";

export interface RecursiveClientOptions {
  /** URL of the Recursive server. Default: `RECURSIVE_BASE_URL` env or `http://127.0.0.1:3000`. */
  baseUrl?: string;
  /** API key. Default: `RECURSIVE_API_KEY` env var. */
  apiKey?: string;
}

export class RecursiveClient {
  readonly baseUrl: string;
  private readonly _http: HttpClient;

  constructor(options: RecursiveClientOptions = {}) {
    const baseUrl =
      options.baseUrl ??
      process.env["RECURSIVE_BASE_URL"] ??
      "http://127.0.0.1:3000";
    this.baseUrl = baseUrl.replace(/\/$/, "");
    this._http = new HttpClient({ baseUrl, apiKey: options.apiKey });
  }

  // ── Health / introspection ─────────────────────────────────────────────

  /** Fetch the server's `/health` text payload (`"ok"` when healthy). */
  async health(): Promise<string> {
    const url = `${this.baseUrl}/health`;
    const headers: Record<string, string> = {};
    const apiKey = process.env["RECURSIVE_API_KEY"];
    if (apiKey) headers["x-api-key"] = apiKey;
    const resp = await fetch(url, { method: "GET", headers });
    return resp.text();
  }

  /** List tools exposed by the server. */
  async listTools(): Promise<ToolInfo[]> {
    const data = (await this._http.get("/tools")) as Array<
      Record<string, unknown>
    >;
    return data.map((t) => ({
      name: String(t["name"] ?? ""),
      description: String(t["description"] ?? ""),
      parameters: (t["parameters"] as Record<string, unknown>) ?? {},
    }));
  }

  // ── Sessions ───────────────────────────────────────────────────────────

  /** List active sessions. */
  async listSessions(): Promise<SessionInfo[]> {
    const data = (await this._http.get("/sessions")) as Array<
      Record<string, unknown>
    >;
    return data.map((s) => ({
      id: String(s["id"]),
      createdAt: String(s["created_at"] ?? ""),
      messageCount: Number(s["message_count"] ?? 0),
      lastPrompt: s["last_prompt"] as string | undefined,
      firstPrompt: s["first_prompt"] as string | undefined,
      goal: s["goal"] as string | undefined,
    }));
  }

  /**
   * Get full session detail including transcript, status, and (when active)
   * the pending plan, goal-loop state, and todo list.
   */
  async getSession(sessionId: string): Promise<SessionDetail> {
    const data = (await this._http.get(
      `/sessions/${sessionId}`,
    )) as Record<string, unknown>;

    const detail: SessionDetail = {
      id: String(data["id"] ?? sessionId),
      createdAt: String(data["created_at"] ?? ""),
      messages: (data["messages"] as unknown[]) ?? [],
      status: String(data["status"] ?? "idle"),
    };

    if (data["pending_plan"] != null) {
      detail.pendingPlan = String(data["pending_plan"]);
    }
    if (data["todos"] != null) {
      detail.todos = data["todos"] as unknown[];
    }
    const goal = data["goal"];
    if (goal && typeof goal === "object") {
      detail.goal = parseGoalState(goal as Record<string, unknown>);
    }
    return detail;
  }

  /** Delete a session by ID. */
  async deleteSession(sessionId: string): Promise<void> {
    await this._http.delete(`/sessions/${sessionId}`);
  }

  // ── Plan Mode 2.0 (g165–167) ──────────────────────────────────────────

  /**
   * Approve the pending plan for a session in `plan_pending_approval` state.
   *
   * @param edits  Optional replacement plan text.
   */
  async approvePlan(
    sessionId: string,
    options: { edits?: string } = {},
  ): Promise<PlanApprovalResponse> {
    const body: Record<string, unknown> = {};
    if (options.edits != null) body["edits"] = options.edits;
    const data = (await this._http.post(
      `/sessions/${sessionId}/plan/confirm`,
      body,
    )) as Record<string, unknown>;
    return {
      status: (data["status"] as "approved" | "rejected") ?? "approved",
      sessionId: String(data["session_id"] ?? sessionId),
    };
  }

  /**
   * Reject the pending plan for a session.
   *
   * @param reason  Reason shown to the agent on the next turn.
   */
  async rejectPlan(
    sessionId: string,
    options: { reason?: string } = {},
  ): Promise<PlanApprovalResponse> {
    const data = (await this._http.post(
      `/sessions/${sessionId}/plan/reject`,
      { reason: options.reason ?? "" },
    )) as Record<string, unknown>;
    return {
      status: (data["status"] as "approved" | "rejected") ?? "rejected",
      sessionId: String(data["session_id"] ?? sessionId),
    };
  }

  // ── Goal-168: goal-loop ───────────────────────────────────────────────

  /**
   * Start a condition-based autonomous loop. The server runs agent turns
   * and evaluates `condition` after each one until it is met or `maxTurns`
   * is exhausted.
   */
  async setGoal(
    sessionId: string,
    condition: string,
    options: { maxTurns?: number } = {},
  ): Promise<GoalActionResponse> {
    const data = (await this._http.post(
      `/sessions/${sessionId}/goal`,
      { condition, max_turns: options.maxTurns ?? 20 },
    )) as Record<string, unknown>;
    return {
      status: String(data["status"] ?? "pursuing"),
      sessionId: String(data["session_id"] ?? sessionId),
    };
  }

  /** Clear the active goal for a session. */
  async clearGoal(sessionId: string): Promise<GoalActionResponse> {
    // HttpClient.delete() returns void — call fetch directly to capture body.
    const url = `${this.baseUrl}/sessions/${sessionId}/goal`;
    const headers: Record<string, string> = {
      "Content-Type": "application/json",
    };
    const apiKey = process.env["RECURSIVE_API_KEY"];
    if (apiKey) headers["x-api-key"] = apiKey;
    const resp = await fetch(url, { method: "DELETE", headers });
    if (!resp.ok) {
      const text = await resp.text();
      throw new Error(`HTTP ${resp.status}: ${text}`);
    }
    const data = (await resp.json()) as Record<string, unknown>;
    return {
      status: String(data["status"] ?? "cleared"),
      sessionId: String(data["session_id"] ?? sessionId),
    };
  }

  /**
   * Read the active goal for a session, or `null` if no goal is set.
   *
   * Convenience over {@link getSession} for goal-loop polling.
   */
  async getGoal(sessionId: string): Promise<GoalState | null> {
    const detail = await this.getSession(sessionId);
    return detail.goal ?? null;
  }

  // ── Goal-169: slash commands ──────────────────────────────────────────

  /** List all registered slash commands (built-in and skill-backed). */
  async listSlashCommands(): Promise<SlashCommandInfo[]> {
    const data = (await this._http.get("/slash-commands")) as Array<
      Record<string, unknown>
    >;
    return data.map((c) => ({
      name: String(c["name"] ?? ""),
      description: String(c["description"] ?? ""),
      source: String(c["source"] ?? ""),
      aliases: (c["aliases"] as string[]) ?? [],
      argumentHint: String(c["argument_hint"] ?? ""),
    }));
  }
}

// ── helpers ────────────────────────────────────────────────────────────────

function parseGoalState(raw: Record<string, unknown>): GoalState {
  return {
    condition: String(raw["condition"] ?? ""),
    status: String(raw["status"] ?? ""),
    turns: Number(raw["turns"] ?? 0),
    maxTurns: Number(raw["max_turns"] ?? 0),
    lastReason: raw["last_reason"] as string | undefined,
  };
}
