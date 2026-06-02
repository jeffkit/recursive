// ── Message content types ─────────────────────────────────────────────────

export interface TextContent {
  type: "text";
  text: string;
}

export interface ToolUseBlock {
  type: "tool_use";
  id: string;
  name: string;
  input: Record<string, unknown>;
}

export interface ToolResultBlock {
  type: "tool_result";
  tool_use_id: string;
  content: string;
}

export type ContentBlock = TextContent | ToolUseBlock | ToolResultBlock;

// ── Messages ──────────────────────────────────────────────────────────────

export interface AssistantMessage {
  type: "assistant";
  content: ContentBlock[];
  sessionId: string;
}

export interface UserMessage {
  type: "user";
  content: string;
  sessionId: string;
}

export interface SystemMessage {
  type: "system";
  subtype: string;
  data: Record<string, unknown>;
}

/**
 * SDK Phase B: emitted when a tool call completes with wall-clock timing.
 *
 * Yielded by `Run.stream()` / `Run.messages()` as `type === "tool_progress"`.
 */
export interface ToolProgressMessage {
  type: "tool_progress";
  /** The tool call ID that just finished. */
  toolUseId: string;
  /** Name of the tool that was called. */
  toolName: string;
  /** Wall-clock milliseconds from tool call start to result receipt. */
  elapsedMs: number;
  sessionId: string;
}

/**
 * SDK Phase C: a streaming text delta from the assistant.
 *
 * Corresponds to `SDKPartialAssistantMessage` in the Claude Agent SDK.
 * Yielded by `Run.stream()` / `Run.messages()` as `type === "stream_event"`.
 *
 * Callers that want token-level granularity (e.g. typewriter UI) can filter:
 * ```ts
 * for await (const msg of run.stream()) {
 *   if (msg.type === "stream_event") process.stdout.write(msg.text);
 * }
 * ```
 *
 * Most callers should use the full `AssistantMessage` (`type === "assistant"`),
 * which is emitted once the entire turn is complete.
 */
export interface PartialAssistantMessage {
  type: "stream_event";
  /** The token delta text. */
  text: string;
  /** Agent step index — use to group deltas from the same turn. */
  step: number;
  sessionId: string;
}

export type SDKMessage =
  | AssistantMessage
  | UserMessage
  | SystemMessage
  | ToolProgressMessage
  | PartialAssistantMessage;

// ── Run result ────────────────────────────────────────────────────────────

export interface UsageMeta {
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
export type RunSubtype =
  | "success"
  | "error_max_turns"
  | "error_during_execution"
  | "cancelled";

/** @internal Map Rust FinishReason debug strings to RunSubtype. */
export function mapFinishReasonToSubtype(
  finishReason: string | undefined,
  status: "finished" | "error" | "cancelled",
): RunSubtype {
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

export interface RunResult {
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

// ── Session info ──────────────────────────────────────────────────────────

export interface SessionInfo {
  id: string;
  createdAt: string;
  messageCount: number;
  lastPrompt?: string;
  firstPrompt?: string;
  goal?: string;
  /** Optional human-readable title, set via `Agent.renameSession()`. */
  title?: string;
}

// ── Tool info (Goal-169 / /tools) ─────────────────────────────────────────

export interface ToolInfo {
  name: string;
  description: string;
  parameters: Record<string, unknown>;
}

// ── Session detail (with optional plan / goal / todos) ────────────────────

/**
 * Full session detail, as returned by `GET /sessions/{id}`.
 *
 * `pendingPlan` is set when the session is in `plan_pending_approval` state
 * (Plan Mode 2.0 — g165–167). `goal` is set when an autonomous goal loop is
 * active (g168). `todos` carries the todo_write task list (g167) when present.
 */
export interface SessionDetail {
  id: string;
  createdAt: string;
  /** Optional human-readable title. */
  title?: string;
  messages: unknown[];
  status: string;
  pendingPlan?: string;
  goal?: GoalState;
  todos?: unknown[];
  /** First user message in the session. */
  firstPrompt?: string;
  /** Most recent user message in the session. */
  lastPrompt?: string;
}

// ── Plan Mode 2.0 (g165–167) ──────────────────────────────────────────────

export interface PlanApprovalResponse {
  status: "approved" | "rejected";
  sessionId: string;
}

// ── Goal-168: goal-loop ──────────────────────────────────────────────────

export interface GoalState {
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

export interface GoalActionResponse {
  /** "pursuing" | "cleared" | "achieved" */
  status: string;
  sessionId: string;
}

// ── Goal-169: slash commands ─────────────────────────────────────────────

export interface SlashCommandInfo {
  name: string;
  description: string;
  /** "builtin" | "skill" */
  source: string;
  aliases: string[];
  argumentHint: string;
}
