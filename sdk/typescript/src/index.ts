/**
 * @recursive/sdk — TypeScript SDK for the Recursive Agent.
 *
 * Claude Agent SDK–compatible `query()` (recommended):
 * ```ts
 * import { query } from "@recursive/sdk";
 *
 * for await (const message of query({
 *   prompt: "List all TODO comments",
 *   options: { maxTurns: 10 },
 * })) {
 *   if (message.type === "result") console.log(message.result);
 * }
 * ```
 *
 * Session-style API (also available):
 * ```ts
 * import { Agent } from "@recursive/sdk";
 * const result = await Agent.prompt("List all TODO comments");
 * ```
 *
 * Environment variables:
 * - `RECURSIVE_BIN` — path to the `recursive` binary (CLI transport)
 * - `RECURSIVE_BASE_URL` — when set, `Agent.*` uses HTTP instead of CLI
 * - `RECURSIVE_API_KEY` — API key for authenticated HTTP servers
 */

export { Agent, AgentSession } from "./agent.js";
export type { AgentOptions, PromptOptions } from "./agent.js";
export { findRecursiveBinary } from "./binary.js";
export { RecursiveClient } from "./client.js";
export type { RecursiveClientOptions } from "./client.js";
export { RecursiveAgentError } from "./exceptions.js";
export { mapFinishReasonToSubtype } from "./models.js";
export type {
  AssistantMessage,
  ContentBlock,
  GoalActionResponse,
  GoalState,
  PartialAssistantMessage,
  PlanApprovalResponse,
  RunResult,
  RunSubtype,
  SDKMessage,
  SessionDetail,
  SessionInfo,
  SlashCommandInfo,
  SystemMessage,
  TextContent,
  ToolInfo,
  ToolProgressMessage,
  ToolResultBlock,
  ToolUseBlock,
  UsageMeta,
  UserMessage,
} from "./models.js";
export { query } from "./query.js";
export type {
  CanUseTool,
  HookCallback,
  Options,
  PermissionResult,
  Query,
  QueryMessage,
} from "./query.js";
export { Run } from "./run.js";
export { buildControlCliArgs } from "./controlSession.js";
export { buildCliArgs } from "./subprocess.js";
export { parseWireObject } from "./wire.js";
