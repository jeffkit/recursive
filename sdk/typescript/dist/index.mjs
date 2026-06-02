// src/exceptions.ts
var RecursiveAgentError = class extends Error {
  constructor(message, options) {
    super(message);
    this.name = "RecursiveAgentError";
    this.isRetryable = options?.isRetryable ?? false;
    Object.setPrototypeOf(this, new.target.prototype);
  }
};

// src/sse.ts
async function* parseSse(lines) {
  let eventType = "message";
  const dataParts = [];
  for await (const line of lines) {
    if (line === "") {
      if (dataParts.length > 0) {
        const payload = dataParts.join("\n");
        let parsed;
        try {
          parsed = JSON.parse(payload);
        } catch {
          parsed = { raw: payload };
        }
        yield { type: eventType, data: parsed };
      }
      eventType = "message";
      dataParts.length = 0;
      continue;
    }
    if (line.startsWith("event:")) {
      eventType = line.slice(6).trim();
    } else if (line.startsWith("data:")) {
      dataParts.push(line.slice(5).trim());
    }
  }
}
async function* streamToLines(body) {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      const parts = buffer.split("\n");
      buffer = parts.pop() ?? "";
      for (const part of parts) {
        yield part;
      }
    }
    if (buffer) yield buffer;
  } finally {
    reader.releaseLock();
  }
}

// src/http.ts
var HttpClient = class {
  constructor({ baseUrl, apiKey }) {
    this.baseUrl = baseUrl.replace(/\/$/, "");
    this.headers = {
      "Content-Type": "application/json"
    };
    const key = apiKey ?? process.env["RECURSIVE_API_KEY"];
    if (key) {
      this.headers["x-api-key"] = key;
    }
  }
  async get(path) {
    const resp = await this._fetch("GET", path);
    return resp.json();
  }
  async post(path, body) {
    const resp = await this._fetch("POST", path, body);
    return resp.json();
  }
  async patch(path, body) {
    const resp = await this._fetch("PATCH", path, body);
    return resp.json();
  }
  async delete(path) {
    await this._fetch("DELETE", path);
  }
  async *streamEvents(path) {
    let resp;
    try {
      resp = await fetch(`${this.baseUrl}${path}`, {
        method: "GET",
        headers: { ...this.headers, Accept: "text/event-stream" }
      });
    } catch (err) {
      throw new RecursiveAgentError(
        `Cannot reach Recursive server at ${this.baseUrl}: ${err}`,
        { isRetryable: true }
      );
    }
    if (!resp.ok) {
      const text = await resp.text();
      throw new RecursiveAgentError(
        `HTTP ${resp.status}: ${text}`,
        { isRetryable: resp.status >= 500 }
      );
    }
    if (!resp.body) {
      throw new RecursiveAgentError("Response body is null");
    }
    const body = resp.body;
    try {
      yield* parseSse(streamToLines(body));
    } finally {
      try {
        await body.cancel();
      } catch {
      }
    }
  }
  async _fetch(method, path, body) {
    try {
      const resp = await fetch(`${this.baseUrl}${path}`, {
        method,
        headers: this.headers,
        body: body !== void 0 ? JSON.stringify(body) : void 0
      });
      if (!resp.ok) {
        const text = await resp.text();
        throw new RecursiveAgentError(
          `HTTP ${resp.status}: ${text}`,
          { isRetryable: resp.status >= 500 }
        );
      }
      return resp;
    } catch (err) {
      if (err instanceof RecursiveAgentError) throw err;
      throw new RecursiveAgentError(
        `Cannot reach Recursive server at ${this.baseUrl}: ${err}`,
        { isRetryable: true }
      );
    }
  }
};

// src/models.ts
function mapFinishReasonToSubtype(finishReason, status) {
  if (status === "cancelled") return "cancelled";
  if (!finishReason) return status === "finished" ? "success" : "error_during_execution";
  if (finishReason.includes("BudgetExceeded") || finishReason.includes("TranscriptLimit")) {
    return "error_max_turns";
  }
  if (finishReason.includes("Cancelled")) return "cancelled";
  if (finishReason.includes("NoMoreToolCalls") || finishReason.includes("PlanPending")) {
    return "success";
  }
  return status === "finished" ? "success" : "error_during_execution";
}

// src/run.ts
var Run = class {
  constructor(sessionId, http) {
    this._result = null;
    /**
     * Pending send POST issued by `Agent.send()`. Set by the Agent factory
     * after construction; awaited inside `wait()` so callers don't have to
     * separately await the dispatch.
     * @internal
     */
    this._sendPromise = null;
    this.id = sessionId;
    this._http = http;
  }
  /**
   * Used by `Agent.send()` to record a failed POST. The error becomes
   * the `RunResult` when `wait()` is next called.
   * @internal
   */
  _fail(err) {
    if (this._result === null) {
      const msg = err instanceof Error ? err.message : `failed to send message: ${err}`;
      this._result = {
        id: this.id,
        status: "error",
        subtype: "error_during_execution",
        error: msg,
        ok: false
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
  async *stream() {
    let finishReason;
    let usageData;
    let runStatus = "finished";
    const resultParts = [];
    let numTurns = 0;
    const startMs = Date.now();
    for await (const event of this._http.streamEvents(
      `/sessions/${this.id}/events`
    )) {
      const evType = event.type;
      const data = event.data;
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
        yield {
          type: "system",
          subtype: "partial_message",
          data
        };
      } else if (evType === "tool_progress") {
        const tp = {
          type: "tool_progress",
          toolUseId: String(data["tool_use_id"] ?? ""),
          toolName: String(data["tool_name"] ?? ""),
          elapsedMs: Number(data["elapsed_ms"] ?? 0),
          sessionId: this.id
        };
        yield tp;
      } else if (evType === "done") {
        finishReason = data["finish_reason"];
        usageData = data["usage"];
        runStatus = data["status"] ?? "finished";
        break;
      } else if (evType === "error") {
        runStatus = "error";
        this._result = {
          id: this.id,
          status: "error",
          subtype: "error_during_execution",
          error: String(data["message"] ?? data),
          ok: false,
          numTurns,
          durationMs: Date.now() - startMs
        };
        return;
      }
    }
    const usage = usageData ? parseUsage(usageData) : void 0;
    this._result = {
      id: this.id,
      status: runStatus,
      subtype: mapFinishReasonToSubtype(finishReason, runStatus),
      finishReason,
      usage,
      ok: runStatus === "finished",
      result: resultParts.length > 0 ? resultParts.join("") : void 0,
      numTurns,
      durationMs: Date.now() - startMs
    };
  }
  /**
   * Alias for `stream()` — matches the Claude Agent SDK naming.
   */
  messages() {
    return this.stream();
  }
  /**
   * Async generator that yields only text chunks from assistant messages.
   */
  async *iterText() {
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
  async text() {
    const chunks = [];
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
  async wait() {
    if (this._result === null) {
      for await (const _ of this.stream()) {
      }
    }
    if (this._sendPromise) {
      await this._sendPromise;
    }
    return this._result;
  }
  // ── cancel ────────────────────────────────────────────────────────────────
  /**
   * Request cancellation of the current run.
   *
   * Sends `POST /sessions/:id/interrupt` to ask the server to stop the active
   * agent turn as soon as possible.  The stream will eventually close with
   * `status === "cancelled"`.  Best-effort — does not throw on network errors.
   */
  async cancel() {
    try {
      await this._http.post(`/sessions/${this.id}/interrupt`, {});
    } catch {
    }
  }
  // ── supports ─────────────────────────────────────────────────────────────
  /**
   * Check whether *operation* is supported for this run.
   */
  supports(operation) {
    return ["stream", "messages", "iterText", "text", "wait", "cancel"].includes(
      operation
    );
  }
};
function parseMessage(data, sessionId) {
  const role = data["role"];
  const contentRaw = data["content"];
  if (role === "assistant") {
    const content = [];
    if (typeof contentRaw === "string") {
      content.push({ type: "text", text: contentRaw });
    } else if (Array.isArray(contentRaw)) {
      for (const item of contentRaw) {
        const t = item["type"];
        if (t === "text") {
          content.push({ type: "text", text: String(item["text"] ?? "") });
        } else if (t === "tool_use") {
          content.push({
            type: "tool_use",
            id: String(item["id"] ?? ""),
            name: String(item["name"] ?? ""),
            input: item["input"] ?? {}
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
      sessionId
    };
  }
  if (role === "system" || data["type"] === "system") {
    return {
      type: "system",
      subtype: String(data["subtype"] ?? ""),
      data
    };
  }
  return null;
}
function parseUsage(data) {
  return {
    inputTokens: Number(data["input_tokens"] ?? 0),
    outputTokens: Number(data["output_tokens"] ?? 0),
    cacheCreationTokens: data["cache_creation_tokens"] != null ? Number(data["cache_creation_tokens"]) : void 0,
    cacheReadTokens: data["cache_read_tokens"] != null ? Number(data["cache_read_tokens"]) : void 0,
    reasoningTokens: data["reasoning_tokens"] != null ? Number(data["reasoning_tokens"]) : void 0
  };
}

// src/agent.ts
var AgentSession = class {
  constructor(sessionId, http, options) {
    this._closed = false;
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
  async send(message) {
    if (this._closed) {
      throw new RecursiveAgentError("Agent session is already closed.");
    }
    const run = new Run(this.sessionId, this._http);
    const sendPromise = this._http.post(`/sessions/${this.sessionId}/messages`, { content: message }).catch((err) => {
      run._fail(err);
    });
    run._sendPromise = sendPromise;
    return run;
  }
  // ── disposal ─────────────────────────────────────────────────────────────
  async close() {
    if (!this._closed) {
      this._closed = true;
      if (this._ownsSession) {
        try {
          await this._http.delete(`/sessions/${this.sessionId}`);
        } catch {
        }
      }
    }
  }
  /** `Symbol.asyncDispose` support — use with `await using`. */
  async [Symbol.asyncDispose]() {
    return this.close();
  }
};
var Agent = class {
  /** Create a new agent session. */
  static async create(options = {}) {
    const http = makeClient(options);
    const body = {};
    if (options.systemPrompt) body["system_prompt"] = options.systemPrompt;
    const data = await http.post("/sessions", body);
    return new AgentSession(data.id, http, { ownsSession: true });
  }
  /**
   * Resume an existing session by ID.
   *
   * The session is **not deleted** on close (since we don't own it).
   */
  static async resume(sessionId, options = {}) {
    const http = makeClient(options);
    await http.get(`/sessions/${sessionId}`);
    return new AgentSession(sessionId, http, { ownsSession: false });
  }
  /**
   * One-shot convenience: create a session, send *message*, wait, clean up.
   *
   * Returns a `RunResult`.
   */
  static async prompt(message, options = {}) {
    const http = makeClient(options);
    const body = { goal: message };
    if (options.systemPrompt) body["system_prompt"] = options.systemPrompt;
    if (options.maxSteps != null) body["max_steps"] = options.maxSteps;
    const data = await http.post("/run", body);
    const usageRaw = data["usage"];
    const status = data["status"] ?? "finished";
    const finishReason = data["finish_reason"];
    return {
      id: String(data["session_id"] ?? ""),
      status,
      subtype: mapFinishReasonToSubtype(finishReason, status),
      finishReason,
      error: data["error"],
      usage: usageRaw ? {
        inputTokens: Number(usageRaw["input_tokens"] ?? 0),
        outputTokens: Number(usageRaw["output_tokens"] ?? 0)
      } : void 0,
      ok: status === "finished"
    };
  }
  /**
   * List active sessions, with optional pagination.
   *
   * @param pagination - Optional `limit` and `offset` query params.
   * @param options - Connection options.
   */
  static async listSessions(pagination = {}, options = {}) {
    const http = makeClient(options);
    const params = new URLSearchParams();
    if (pagination.limit != null) params.set("limit", String(pagination.limit));
    if (pagination.offset != null) params.set("offset", String(pagination.offset));
    const url = params.size > 0 ? `/sessions?${params}` : "/sessions";
    const data = await http.get(url);
    return data.map((s) => ({
      id: String(s["id"]),
      createdAt: String(s["created_at"] ?? ""),
      messageCount: Number(s["message_count"] ?? 0),
      lastPrompt: s["last_prompt"],
      firstPrompt: s["first_prompt"],
      goal: s["goal"],
      title: s["title"]
    }));
  }
  /**
   * Set a human-readable title for a session.
   *
   * Calls `PATCH /sessions/:id` with `{"title": title}`.
   * Pass an empty string to clear the title.
   */
  static async renameSession(sessionId, title, options = {}) {
    const http = makeClient(options);
    await http.patch(`/sessions/${sessionId}`, { title });
  }
  /**
   * Return the transcript messages for a session.
   *
   * Fetches `GET /sessions/:id` and returns the `messages` array.
   * Each message is a raw object with at minimum `role` and `content` fields.
   *
   * ```ts
   * const msgs = await Agent.getSessionMessages(sessionId);
   * for (const m of msgs) {
   *   console.log(m["role"], String(m["content"]).slice(0, 60));
   * }
   * ```
   */
  static async getSessionMessages(sessionId, options = {}) {
    const http = makeClient(options);
    const data = await http.get(`/sessions/${sessionId}`);
    return data["messages"] ?? [];
  }
  /** Delete a session by ID. */
  static async deleteSession(sessionId, options = {}) {
    const http = makeClient(options);
    await http.delete(`/sessions/${sessionId}`);
  }
};
function makeClient(options) {
  const baseUrl = options.baseUrl ?? process.env["RECURSIVE_BASE_URL"] ?? "http://127.0.0.1:3000";
  return new HttpClient({ baseUrl, apiKey: options.apiKey });
}

// src/client.ts
var RecursiveClient = class {
  constructor(options = {}) {
    const baseUrl = options.baseUrl ?? process.env["RECURSIVE_BASE_URL"] ?? "http://127.0.0.1:3000";
    this.baseUrl = baseUrl.replace(/\/$/, "");
    this._http = new HttpClient({ baseUrl, apiKey: options.apiKey });
  }
  // ── Health / introspection ─────────────────────────────────────────────
  /** Fetch the server's `/health` text payload (`"ok"` when healthy). */
  async health() {
    const url = `${this.baseUrl}/health`;
    const headers = {};
    const apiKey = process.env["RECURSIVE_API_KEY"];
    if (apiKey) headers["x-api-key"] = apiKey;
    const resp = await fetch(url, { method: "GET", headers });
    return resp.text();
  }
  /** List tools exposed by the server. */
  async listTools() {
    const data = await this._http.get("/tools");
    return data.map((t) => ({
      name: String(t["name"] ?? ""),
      description: String(t["description"] ?? ""),
      parameters: t["parameters"] ?? {}
    }));
  }
  // ── Sessions ───────────────────────────────────────────────────────────
  /** List active sessions. */
  async listSessions() {
    const data = await this._http.get("/sessions");
    return data.map((s) => ({
      id: String(s["id"]),
      createdAt: String(s["created_at"] ?? ""),
      messageCount: Number(s["message_count"] ?? 0),
      lastPrompt: s["last_prompt"],
      firstPrompt: s["first_prompt"],
      goal: s["goal"]
    }));
  }
  /**
   * Get full session detail including transcript, status, and (when active)
   * the pending plan, goal-loop state, and todo list.
   */
  async getSession(sessionId) {
    const data = await this._http.get(
      `/sessions/${sessionId}`
    );
    const detail = {
      id: String(data["id"] ?? sessionId),
      createdAt: String(data["created_at"] ?? ""),
      messages: data["messages"] ?? [],
      status: String(data["status"] ?? "idle")
    };
    if (data["pending_plan"] != null) {
      detail.pendingPlan = String(data["pending_plan"]);
    }
    if (data["todos"] != null) {
      detail.todos = data["todos"];
    }
    const goal = data["goal"];
    if (goal && typeof goal === "object") {
      detail.goal = parseGoalState(goal);
    }
    return detail;
  }
  /** Delete a session by ID. */
  async deleteSession(sessionId) {
    await this._http.delete(`/sessions/${sessionId}`);
  }
  /**
   * Return the transcript messages for a session.
   *
   * Each message is a raw object with at minimum `role` and `content` keys.
   * This is a convenience wrapper over {@link getSession} for callers that
   * only need the message history.
   */
  async getSessionMessages(sessionId) {
    const detail = await this.getSession(sessionId);
    return detail.messages;
  }
  // ── Plan Mode 2.0 (g165–167) ──────────────────────────────────────────
  /**
   * Approve the pending plan for a session in `plan_pending_approval` state.
   *
   * @param edits  Optional replacement plan text.
   */
  async approvePlan(sessionId, options = {}) {
    const body = {};
    if (options.edits != null) body["edits"] = options.edits;
    const data = await this._http.post(
      `/sessions/${sessionId}/plan/confirm`,
      body
    );
    return {
      status: data["status"] ?? "approved",
      sessionId: String(data["session_id"] ?? sessionId)
    };
  }
  /**
   * Reject the pending plan for a session.
   *
   * @param reason  Reason shown to the agent on the next turn.
   */
  async rejectPlan(sessionId, options = {}) {
    const data = await this._http.post(
      `/sessions/${sessionId}/plan/reject`,
      { reason: options.reason ?? "" }
    );
    return {
      status: data["status"] ?? "rejected",
      sessionId: String(data["session_id"] ?? sessionId)
    };
  }
  // ── Goal-168: goal-loop ───────────────────────────────────────────────
  /**
   * Start a condition-based autonomous loop. The server runs agent turns
   * and evaluates `condition` after each one until it is met or `maxTurns`
   * is exhausted.
   */
  async setGoal(sessionId, condition, options = {}) {
    const data = await this._http.post(
      `/sessions/${sessionId}/goal`,
      { condition, max_turns: options.maxTurns ?? 20 }
    );
    return {
      status: String(data["status"] ?? "pursuing"),
      sessionId: String(data["session_id"] ?? sessionId)
    };
  }
  /** Clear the active goal for a session. */
  async clearGoal(sessionId) {
    const url = `${this.baseUrl}/sessions/${sessionId}/goal`;
    const headers = {
      "Content-Type": "application/json"
    };
    const apiKey = process.env["RECURSIVE_API_KEY"];
    if (apiKey) headers["x-api-key"] = apiKey;
    const resp = await fetch(url, { method: "DELETE", headers });
    if (!resp.ok) {
      const text = await resp.text();
      throw new Error(`HTTP ${resp.status}: ${text}`);
    }
    const data = await resp.json();
    return {
      status: String(data["status"] ?? "cleared"),
      sessionId: String(data["session_id"] ?? sessionId)
    };
  }
  /**
   * Read the active goal for a session, or `null` if no goal is set.
   *
   * Convenience over {@link getSession} for goal-loop polling.
   */
  async getGoal(sessionId) {
    const detail = await this.getSession(sessionId);
    return detail.goal ?? null;
  }
  // ── Goal-169: slash commands ──────────────────────────────────────────
  /** List all registered slash commands (built-in and skill-backed). */
  async listSlashCommands() {
    const data = await this._http.get("/slash-commands");
    return data.map((c) => ({
      name: String(c["name"] ?? ""),
      description: String(c["description"] ?? ""),
      source: String(c["source"] ?? ""),
      aliases: c["aliases"] ?? [],
      argumentHint: String(c["argument_hint"] ?? "")
    }));
  }
};
function parseGoalState(raw) {
  return {
    condition: String(raw["condition"] ?? ""),
    status: String(raw["status"] ?? ""),
    turns: Number(raw["turns"] ?? 0),
    maxTurns: Number(raw["max_turns"] ?? 0),
    lastReason: raw["last_reason"]
  };
}
export {
  Agent,
  AgentSession,
  RecursiveAgentError,
  RecursiveClient,
  Run,
  mapFinishReasonToSubtype
};
