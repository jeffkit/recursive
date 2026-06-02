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

export type SDKMessage = AssistantMessage | UserMessage | SystemMessage;

// ── Run result ────────────────────────────────────────────────────────────

export interface UsageMeta {
  inputTokens: number;
  outputTokens: number;
  cacheCreationTokens?: number;
  cacheReadTokens?: number;
  reasoningTokens?: number;
}

export interface RunResult {
  /** Session ID. */
  id: string;
  /** `"finished"` | `"error"` | `"cancelled"` */
  status: "finished" | "error" | "cancelled";
  finishReason?: string;
  usage?: UsageMeta;
  error?: string;
  /** Shorthand: `status === "finished"`. */
  ok: boolean;
}

// ── Session info ──────────────────────────────────────────────────────────

export interface SessionInfo {
  id: string;
  createdAt: string;
  messageCount: number;
  lastPrompt?: string;
  firstPrompt?: string;
  goal?: string;
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
  messages: unknown[];
  status: string;
  pendingPlan?: string;
  goal?: GoalState;
  todos?: unknown[];
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
