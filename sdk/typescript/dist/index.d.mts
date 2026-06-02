/**
 * Minimal SSE parser for Node.js streams.
 *
 * Reads lines from an async iterable (e.g. a fetch response body) and yields
 * parsed `{ type, data }` event objects.
 */
interface SseEvent {
    type: string;
    data: unknown;
}

/**
 * Internal HTTP client (not part of the public API).
 */

interface HttpClientOptions {
    baseUrl: string;
    apiKey?: string;
    timeout?: number;
}
declare class HttpClient {
    readonly baseUrl: string;
    private readonly headers;
    constructor({ baseUrl, apiKey }: HttpClientOptions);
    get(path: string): Promise<unknown>;
    post(path: string, body: unknown): Promise<unknown>;
    delete(path: string): Promise<void>;
    streamEvents(path: string): AsyncGenerator<SseEvent>;
    private _fetch;
}

interface TextContent {
    type: "text";
    text: string;
}
interface ToolUseBlock {
    type: "tool_use";
    id: string;
    name: string;
    input: Record<string, unknown>;
}
interface ToolResultBlock {
    type: "tool_result";
    tool_use_id: string;
    content: string;
}
type ContentBlock = TextContent | ToolUseBlock | ToolResultBlock;
interface AssistantMessage {
    type: "assistant";
    content: ContentBlock[];
    sessionId: string;
}
interface UserMessage {
    type: "user";
    content: string;
    sessionId: string;
}
interface SystemMessage {
    type: "system";
    subtype: string;
    data: Record<string, unknown>;
}
/**
 * SDK Phase B: emitted when a tool call completes with wall-clock timing.
 *
 * Yielded by `Run.stream()` / `Run.messages()` as `type === "tool_progress"`.
 */
interface ToolProgressMessage {
    type: "tool_progress";
    /** The tool call ID that just finished. */
    toolUseId: string;
    /** Name of the tool that was called. */
    toolName: string;
    /** Wall-clock milliseconds from tool call start to result receipt. */
    elapsedMs: number;
    sessionId: string;
}
type SDKMessage = AssistantMessage | UserMessage | SystemMessage | ToolProgressMessage;
interface UsageMeta {
    inputTokens: number;
    outputTokens: number;
    cacheCreationTokens?: number;
    cacheReadTokens?: number;
    reasoningTokens?: number;
}
/**
 * Claude Agent SDK–compatible result subtype.
 *
 * Maps the Rust `finish_reason` debug string to a portable label:
 * - `"success"` — normal completion
 * - `"error_max_turns"` — turn budget or transcript size exceeded
 * - `"error_during_execution"` — stuck loop, provider stop, etc.
 * - `"cancelled"` — interrupted / cancelled
 */
type RunSubtype = "success" | "error_max_turns" | "error_during_execution" | "cancelled";
/** @internal Map Rust FinishReason debug strings to RunSubtype. */
declare function mapFinishReasonToSubtype(finishReason: string | undefined, status: "finished" | "error" | "cancelled"): RunSubtype;
interface RunResult {
    /** Session ID. */
    id: string;
    /** `"finished"` | `"error"` | `"cancelled"` */
    status: "finished" | "error" | "cancelled";
    /**
     * Claude Agent SDK–compatible result subtype.
     * Derived from `finishReason` and `status`.
     */
    subtype: RunSubtype;
    finishReason?: string;
    usage?: UsageMeta;
    error?: string;
    /** Shorthand: `status === "finished"`. */
    ok: boolean;
    /** Concatenated final assistant text (collected while streaming). */
    result?: string;
    /** Number of assistant turns in this run. */
    numTurns?: number;
    /** Wall-clock duration from first stream read to close, in milliseconds. */
    durationMs?: number;
}
interface SessionInfo {
    id: string;
    createdAt: string;
    messageCount: number;
    lastPrompt?: string;
    firstPrompt?: string;
    goal?: string;
}
interface ToolInfo {
    name: string;
    description: string;
    parameters: Record<string, unknown>;
}
/**
 * Full session detail, as returned by `GET /sessions/{id}`.
 *
 * `pendingPlan` is set when the session is in `plan_pending_approval` state
 * (Plan Mode 2.0 — g165–167). `goal` is set when an autonomous goal loop is
 * active (g168). `todos` carries the todo_write task list (g167) when present.
 */
interface SessionDetail {
    id: string;
    createdAt: string;
    messages: unknown[];
    status: string;
    pendingPlan?: string;
    goal?: GoalState;
    todos?: unknown[];
}
interface PlanApprovalResponse {
    status: "approved" | "rejected";
    sessionId: string;
}
interface GoalState {
    /** Natural-language completion condition. */
    condition: string;
    /** "pursuing" | "achieved" | "cleared" */
    status: string;
    /** Turns taken so far. */
    turns: number;
    /** Hard cap on autonomous turns. */
    maxTurns: number;
    /** Human-readable reason for the last status transition. */
    lastReason?: string;
}
interface GoalActionResponse {
    /** "pursuing" | "cleared" | "achieved" */
    status: string;
    sessionId: string;
}
interface SlashCommandInfo {
    name: string;
    description: string;
    /** "builtin" | "skill" */
    source: string;
    aliases: string[];
    argumentHint: string;
}

/**
 * Run — represents a single agent turn in a session.
 */

declare class Run {
    /**
     * Session ID for this run.
     */
    readonly id: string;
    private readonly _http;
    private _result;
    /**
     * Pending send POST issued by `Agent.send()`. Set by the Agent factory
     * after construction; awaited inside `wait()` so callers don't have to
     * separately await the dispatch.
     * @internal
     */
    _sendPromise: Promise<unknown> | null;
    constructor(sessionId: string, http: HttpClient);
    /**
     * Used by `Agent.send()` to record a failed POST. The error becomes
     * the `RunResult` when `wait()` is next called.
     * @internal
     */
    _fail(err: unknown): void;
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
    stream(): AsyncGenerator<SDKMessage>;
    /**
     * Alias for `stream()` — matches the Claude Agent SDK naming.
     */
    messages(): AsyncGenerator<SDKMessage>;
    /**
     * Async generator that yields only text chunks from assistant messages.
     */
    iterText(): AsyncGenerator<string>;
    /**
     * Block until the run completes and return all assistant text concatenated.
     */
    text(): Promise<string>;
    /**
     * Block until the run finishes (drains the stream if not already consumed)
     * and return the terminal `RunResult`.
     *
     * ```ts
     * const result = await run.wait();
     * if (result.status === "error") console.error(result.error);
     * ```
     */
    wait(): Promise<RunResult>;
    /**
     * Request cancellation of the current run.
     *
     * Sends `POST /sessions/:id/interrupt` to ask the server to stop the active
     * agent turn as soon as possible.  The stream will eventually close with
     * `status === "cancelled"`.  Best-effort — does not throw on network errors.
     */
    cancel(): Promise<void>;
    /**
     * Check whether *operation* is supported for this run.
     */
    supports(operation: string): boolean;
}

/**
 * Agent — main entrypoint for the Recursive Agent TypeScript SDK.
 */

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
declare class AgentSession {
    readonly sessionId: string;
    private readonly _http;
    private readonly _ownsSession;
    private _closed;
    constructor(sessionId: string, http: HttpClient, options: {
        ownsSession: boolean;
    });
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
    send(message: string): Promise<Run>;
    close(): Promise<void>;
    /** `Symbol.asyncDispose` support — use with `await using`. */
    [Symbol.asyncDispose](): Promise<void>;
}
interface AgentOptions {
    /** URL of the Recursive server. Default: `RECURSIVE_BASE_URL` env or `http://127.0.0.1:3000`. */
    baseUrl?: string;
    /** API key. Default: `RECURSIVE_API_KEY` env var. */
    apiKey?: string;
    /** HTTP timeout in milliseconds. Default: 120_000. */
    timeout?: number;
    /** System prompt for the session. */
    systemPrompt?: string;
}
interface PromptOptions extends AgentOptions {
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
declare class Agent {
    /** Create a new agent session. */
    static create(options?: AgentOptions): Promise<AgentSession>;
    /**
     * Resume an existing session by ID.
     *
     * The session is **not deleted** on close (since we don't own it).
     */
    static resume(sessionId: string, options?: AgentOptions): Promise<AgentSession>;
    /**
     * One-shot convenience: create a session, send *message*, wait, clean up.
     *
     * Returns a `RunResult`.
     */
    static prompt(message: string, options?: PromptOptions): Promise<RunResult>;
    /** List active sessions. */
    static listSessions(options?: AgentOptions): Promise<SessionInfo[]>;
    /** Delete a session by ID. */
    static deleteSession(sessionId: string, options?: AgentOptions): Promise<void>;
}

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

interface RecursiveClientOptions {
    /** URL of the Recursive server. Default: `RECURSIVE_BASE_URL` env or `http://127.0.0.1:3000`. */
    baseUrl?: string;
    /** API key. Default: `RECURSIVE_API_KEY` env var. */
    apiKey?: string;
}
declare class RecursiveClient {
    readonly baseUrl: string;
    private readonly _http;
    constructor(options?: RecursiveClientOptions);
    /** Fetch the server's `/health` text payload (`"ok"` when healthy). */
    health(): Promise<string>;
    /** List tools exposed by the server. */
    listTools(): Promise<ToolInfo[]>;
    /** List active sessions. */
    listSessions(): Promise<SessionInfo[]>;
    /**
     * Get full session detail including transcript, status, and (when active)
     * the pending plan, goal-loop state, and todo list.
     */
    getSession(sessionId: string): Promise<SessionDetail>;
    /** Delete a session by ID. */
    deleteSession(sessionId: string): Promise<void>;
    /**
     * Return the transcript messages for a session.
     *
     * Each message is a raw object with at minimum `role` and `content` keys.
     * This is a convenience wrapper over {@link getSession} for callers that
     * only need the message history.
     */
    getSessionMessages(sessionId: string): Promise<unknown[]>;
    /**
     * Approve the pending plan for a session in `plan_pending_approval` state.
     *
     * @param edits  Optional replacement plan text.
     */
    approvePlan(sessionId: string, options?: {
        edits?: string;
    }): Promise<PlanApprovalResponse>;
    /**
     * Reject the pending plan for a session.
     *
     * @param reason  Reason shown to the agent on the next turn.
     */
    rejectPlan(sessionId: string, options?: {
        reason?: string;
    }): Promise<PlanApprovalResponse>;
    /**
     * Start a condition-based autonomous loop. The server runs agent turns
     * and evaluates `condition` after each one until it is met or `maxTurns`
     * is exhausted.
     */
    setGoal(sessionId: string, condition: string, options?: {
        maxTurns?: number;
    }): Promise<GoalActionResponse>;
    /** Clear the active goal for a session. */
    clearGoal(sessionId: string): Promise<GoalActionResponse>;
    /**
     * Read the active goal for a session, or `null` if no goal is set.
     *
     * Convenience over {@link getSession} for goal-loop polling.
     */
    getGoal(sessionId: string): Promise<GoalState | null>;
    /** List all registered slash commands (built-in and skill-backed). */
    listSlashCommands(): Promise<SlashCommandInfo[]>;
}

/**
 * Thrown when the agent run could **not start** — auth failure, network error,
 * bad configuration, etc.
 *
 * This is distinct from a run that started but failed (`RunResult.status === "error"`).
 */
declare class RecursiveAgentError extends Error {
    readonly isRetryable: boolean;
    constructor(message: string, options?: {
        isRetryable?: boolean;
    });
}

export { Agent, type AgentOptions, AgentSession, type AssistantMessage, type ContentBlock, type GoalActionResponse, type GoalState, type PlanApprovalResponse, type PromptOptions, RecursiveAgentError, RecursiveClient, type RecursiveClientOptions, Run, type RunResult, type RunSubtype, type SDKMessage, type SessionDetail, type SessionInfo, type SlashCommandInfo, type SystemMessage, type TextContent, type ToolInfo, type ToolProgressMessage, type ToolResultBlock, type ToolUseBlock, type UsageMeta, type UserMessage, mapFinishReasonToSubtype };
