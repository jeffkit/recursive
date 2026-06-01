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
