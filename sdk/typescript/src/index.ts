/**
 * @recursive/sdk — TypeScript SDK for the Recursive Agent.
 *
 * Quick start:
 * ```ts
 * import { Agent } from "@recursive/sdk";
 *
 * // One-shot
 * const result = await Agent.prompt("List all TODO comments", {
 *   baseUrl: "http://localhost:3000",
 * });
 *
 * // Multi-turn with streaming
 * await using agent = await Agent.create({ baseUrl: "http://localhost:3000" });
 * const run = await agent.send("Fix the failing tests");
 * for await (const msg of run.stream()) {
 *   if (msg.type === "assistant") {
 *     for (const b of msg.content) {
 *       if (b.type === "text") process.stdout.write(b.text);
 *     }
 *   }
 * }
 * await run.wait();
 *
 * // Resume
 * await using agent2 = await Agent.resume(sessionId, { baseUrl: "..." });
 * await (await agent2.send("Continue")).wait();
 * ```
 *
 * Environment variables:
 * - `RECURSIVE_BASE_URL` — server URL (default: `http://127.0.0.1:3000`)
 * - `RECURSIVE_API_KEY`  — API key (if auth is enabled)
 */

export { Agent, AgentSession } from "./agent.js";
export type { AgentOptions, PromptOptions } from "./agent.js";
export { RecursiveClient } from "./client.js";
export type { RecursiveClientOptions } from "./client.js";
export { RecursiveAgentError } from "./exceptions.js";
export { mapFinishReasonToSubtype } from "./models.js";
export type {
  AssistantMessage,
  ContentBlock,
  GoalActionResponse,
  GoalState,
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
export { Run } from "./run.js";
