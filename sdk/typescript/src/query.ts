/**
 * Claude Agent SDK–compatible `query()` entrypoint.
 *
 * ```ts
 * import { query } from "@recursive/sdk";
 *
 * for await (const message of query({
 *   prompt: "Find and fix the bug in auth.ts",
 *   options: { maxTurns: 10, permissionMode: "bypassPermissions" },
 * })) {
 *   if (message.type === "assistant") { ... }
 *   if (message.type === "result") { console.log(message.result); }
 * }
 * ```
 *
 * Spawns `recursive -p … --output-format stream-json --input-format stream-json`
 * (no `-H`) so the bidirectional control channel is open for `canUseTool`,
 * hooks, `interrupt`, and streaming-input follow-up turns.
 */

import {
  spawnControlSession,
  type CanUseTool,
  type ControlSessionHandle,
  type ControlSpawnOptions,
  type HookCallback,
  type PermissionResult,
} from "./controlSession.js";
import type { WireItem } from "./wire.js";

// ── Options (Claude Agent SDK–aligned names) ──────────────────────────────

/**
 * Subset of Claude Agent SDK `Options` that Recursive can honour today.
 */
export interface Options {
  /** Working directory for the agent (`--workspace`). */
  cwd?: string;
  /** Model id (`-m`). */
  model?: string;
  /** Max agent steps (`--max-steps`). Claude name: `maxTurns`. */
  maxTurns?: number;
  /**
   * System prompt. String replaces the default; preset object with `append`
   * appends to Recursive's built-in prompt.
   */
  systemPrompt?:
    | string
    | { type: "preset"; preset: "claude_code" | string; append?: string };
  /**
   * Permission mode. Claude values map onto Recursive CLI flags:
   * - `bypassPermissions` / `acceptEdits` / `dontAsk` / `auto` → `auto`
   * - `plan` → plan-first mode
   * - `default` → `default`
   */
  permissionMode?:
    | "default"
    | "acceptEdits"
    | "bypassPermissions"
    | "plan"
    | "dontAsk"
    | "auto"
    | "strict"
    | "bypass";
  /** Resume an existing session id (`-r`). */
  resume?: string;
  /** Path to the `recursive` binary (Claude: `pathToClaudeCodeExecutable`). */
  pathToClaudeCodeExecutable?: string;
  /** Alias for {@link Options.pathToClaudeCodeExecutable}. */
  executable?: string;
  /** AbortController to cancel the run. */
  abortController?: AbortController;
  /** Max API spend in USD (`--max-budget-usd`). */
  maxBudgetUsd?: number;
  /** Restrict tools (`--allow-tools`). */
  allowedTools?: string[];
  /**
   * Host-side tool permission callback (Claude `canUseTool`).
   * Required for interactive prompts; when omitted, tools are auto-allowed
   * under `bypassPermissions`/`auto` (or when listed in `allowedTools`).
   */
  canUseTool?: CanUseTool;
  /**
   * SDK hooks. Recursive forwards `hook_callback` control requests to the
   * registered callbacks after sending `initialize` with callback ids.
   */
  hooks?: {
    [event: string]: Array<{
      matcher?: string;
      hooks: HookCallback[];
    }>;
  };
}

export type { CanUseTool, PermissionResult, HookCallback };

// ── Message types yielded by query() ──────────────────────────────────────

/**
 * Claude-compatible stream message.
 *
 * Includes `type: "result"` as a stream item (Claude Agent SDK behaviour),
 * not a separate `wait()` return value.
 */
export type QueryMessage = Record<string, unknown> & {
  type: string;
  session_id?: string;
};

// ── Query ─────────────────────────────────────────────────────────────────

/**
 * Return type of {@link query}. An async generator of {@link QueryMessage}
 * plus control methods.
 */
export interface Query extends AsyncGenerator<QueryMessage, void> {
  /** Interrupt the running agent (control `interrupt` + SIGTERM). */
  interrupt(): Promise<void>;
  /** Close stdin / tear down the underlying process. */
  close(): void;
  /** Stream additional user turns into the same CLI session. */
  streamInput(prompts: AsyncIterable<string>): Promise<void>;
  /** Host→CLI `set_permission_mode`. */
  setPermissionMode(mode: string): Promise<void>;
  /** Host→CLI `set_model`. */
  setModel(model: string): Promise<void>;
}

/**
 * Run an agent turn and stream Claude-compatible messages.
 *
 * Recursive equivalent of `@anthropic-ai/claude-agent-sdk`'s `query()`.
 */
export function query(params: {
  prompt: string | AsyncIterable<string>;
  options?: Options;
}): Query {
  const options = params.options ?? {};
  const { hookCallbacks, initializeHooks } = materializeHooks(options.hooks);

  const isStreamPrompt =
    typeof params.prompt !== "string" &&
    params.prompt != null &&
    typeof (params.prompt as AsyncIterable<string>)[Symbol.asyncIterator] ===
      "function";

  const initialPrompt = typeof params.prompt === "string" ? params.prompt : "";
  const spawnOpts = optionsToControlSpawn(initialPrompt || " ", options, {
    hookCallbacks,
    initializeHooks,
    keepStdinOpen: isStreamPrompt,
  });

  const handle = spawnControlSession(spawnOpts);

  if (options.abortController) {
    const { signal } = options.abortController;
    if (signal.aborted) {
      handle.cancel();
    } else {
      signal.addEventListener("abort", () => void handle.interrupt(), {
        once: true,
      });
    }
  }

  let streamInputDone: Promise<void> | undefined;
  if (isStreamPrompt) {
    streamInputDone = (async () => {
      for await (const text of params.prompt as AsyncIterable<string>) {
        handle.writeUser(text);
      }
      handle.close();
    })();
  }

  const gen = (async function* (): AsyncGenerator<QueryMessage, void> {
    try {
      for await (const item of handle.items()) {
        const msg = wireItemToQueryMessage(item);
        if (msg) yield msg;
      }
    } finally {
      if (streamInputDone) await streamInputDone.catch(() => undefined);
    }
  })();

  return Object.assign(gen, {
    async interrupt(): Promise<void> {
      await handle.interrupt();
    },
    close(): void {
      handle.close();
      handle.cancel();
    },
    async streamInput(prompts: AsyncIterable<string>): Promise<void> {
      for await (const text of prompts) {
        handle.writeUser(text);
      }
      handle.close();
    },
    async setPermissionMode(mode: string): Promise<void> {
      await handle.setPermissionMode(mode);
    },
    async setModel(model: string): Promise<void> {
      await handle.setModel(model);
    },
  }) as Query;
}

// ── helpers ───────────────────────────────────────────────────────────────

/** @internal — maps Claude options onto control-session spawn opts. */
export function optionsToSpawn(
  prompt: string,
  options: Options,
): ControlSpawnOptions {
  return optionsToControlSpawn(prompt, options, {});
}

function optionsToControlSpawn(
  prompt: string,
  options: Options,
  extras: {
    hookCallbacks?: Map<string, HookCallback>;
    initializeHooks?: Record<string, Array<{ hookCallbackIds: string[] }>>;
    keepStdinOpen?: boolean;
  },
): ControlSpawnOptions {
  let systemPrompt: string | undefined;
  let appendSystemPrompt: string | undefined;
  if (typeof options.systemPrompt === "string") {
    systemPrompt = options.systemPrompt;
  } else if (options.systemPrompt && typeof options.systemPrompt === "object") {
    appendSystemPrompt = options.systemPrompt.append;
  }

  const spawn: ControlSpawnOptions = {
    prompt,
    cwd: options.cwd,
    model: options.model,
    maxSteps: options.maxTurns,
    maxBudgetUsd: options.maxBudgetUsd,
    systemPrompt,
    appendSystemPrompt,
    resumeSessionId: options.resume,
    cliPath: options.pathToClaudeCodeExecutable ?? options.executable,
    permissionMode: mapClaudePermission(options.permissionMode),
    allowedTools: options.allowedTools,
    canUseTool: options.canUseTool,
    hookCallbacks: extras.hookCallbacks,
    initializeHooks: extras.initializeHooks,
    abortSignal: options.abortController?.signal,
    keepStdinOpen: extras.keepStdinOpen,
  };

  if (options.permissionMode === "plan") {
    spawn.planningMode = "plan_first";
    spawn.permissionMode = "default";
  }

  return spawn;
}

function mapClaudePermission(
  mode: Options["permissionMode"],
): ControlSpawnOptions["permissionMode"] {
  switch (mode) {
    case "bypassPermissions":
    case "acceptEdits":
    case "dontAsk":
    case "auto":
    case "bypass":
      return "auto";
    case "plan":
      return "default";
    case "strict":
      return "strict";
    case "default":
    case undefined:
      return "default";
    default:
      return "default";
  }
}

function materializeHooks(hooks: Options["hooks"]): {
  hookCallbacks: Map<string, HookCallback>;
  initializeHooks: Record<string, Array<{ hookCallbackIds: string[] }>>;
} {
  const hookCallbacks = new Map<string, HookCallback>();
  const initializeHooks: Record<
    string,
    Array<{ hookCallbackIds: string[] }>
  > = {};
  if (!hooks) return { hookCallbacks, initializeHooks };

  let n = 0;
  for (const [event, matchers] of Object.entries(hooks)) {
    const ids: string[] = [];
    for (const matcher of matchers) {
      for (const cb of matcher.hooks) {
        const id = `hook_${event}_${n++}`;
        hookCallbacks.set(id, cb);
        ids.push(id);
      }
    }
    if (ids.length > 0) {
      initializeHooks[event] = [{ hookCallbackIds: ids }];
    }
  }
  return { hookCallbacks, initializeHooks };
}

function wireItemToQueryMessage(item: WireItem): QueryMessage | null {
  if (item.kind === "result") {
    const r = item.result;
    const base: QueryMessage = {
      type: "result",
      subtype: r.subtype,
      is_error: !r.ok,
      session_id: r.id,
      num_turns: r.numTurns,
      duration_ms: r.durationMs,
      stop_reason: r.finishReason,
    };
    if (r.result !== undefined) base["result"] = r.result;
    if (r.usage) {
      base["usage"] = {
        input_tokens: r.usage.inputTokens,
        output_tokens: r.usage.outputTokens,
        cache_read_input_tokens: r.usage.cacheReadTokens,
        cache_creation_input_tokens: r.usage.cacheCreationTokens,
      };
    }
    if (r.error) base["errors"] = [r.error];
    return base;
  }

  if (item.kind !== "message") return null;

  const m = item.message;
  if (m.type === "assistant") {
    return {
      type: "assistant",
      session_id: m.sessionId,
      parent_tool_use_id: null,
      message: { role: "assistant", content: m.content },
    };
  }
  if (m.type === "user") {
    return {
      type: "user",
      session_id: m.sessionId,
      parent_tool_use_id: null,
      message: { role: "user", content: m.content },
    };
  }
  if (m.type === "system") {
    return {
      type: "system",
      subtype: m.subtype,
      session_id:
        typeof m.data["session_id"] === "string"
          ? String(m.data["session_id"])
          : undefined,
      ...m.data,
    };
  }
  if (m.type === "stream_event") {
    return {
      type: "stream_event",
      session_id: m.sessionId,
      parent_tool_use_id: null,
      event: {
        type: "content_block_delta",
        index: m.step,
        delta: { type: "text_delta", text: m.text },
      },
    };
  }
  if (m.type === "tool_progress") {
    return {
      type: "tool_progress",
      session_id: m.sessionId,
      tool_use_id: m.toolUseId,
      tool_name: m.toolName,
      elapsed_time_seconds: m.elapsedMs / 1000,
    };
  }
  return null;
}

/** @internal — exported for tests */
export type { ControlSessionHandle };
