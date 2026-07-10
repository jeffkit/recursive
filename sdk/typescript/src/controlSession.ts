/**
 * Bidirectional Claude-compatible control session over `recursive` stdio.
 *
 * Spawns without `-H` so the CLI opens the control channel, then demuxes
 * stdout (`control_request` vs SDK messages) and writes replies on stdin.
 */

import { randomUUID } from "node:crypto";
import { spawn, type ChildProcess } from "node:child_process";
import { createInterface } from "node:readline";

import { findRecursiveBinary } from "./binary.js";
import { RecursiveAgentError } from "./exceptions.js";
import { parseWireObject, type WireItem } from "./wire.js";

/** Claude-style permission decision returned by {@link CanUseTool}. */
export type PermissionResult =
  | { behavior: "allow"; updatedInput?: Record<string, unknown> }
  | { behavior: "deny"; message?: string };

export type CanUseTool = (
  toolName: string,
  input: Record<string, unknown>,
  options: { toolUseId?: string; signal?: AbortSignal },
) => Promise<PermissionResult>;

export type HookCallback = (
  input: Record<string, unknown>,
  toolUseId: string | undefined,
  options: { signal?: AbortSignal },
) => Promise<Record<string, unknown> | void>;

export interface ControlSpawnOptions {
  prompt: string;
  resumeSessionId?: string;
  cwd?: string;
  cliPath?: string;
  model?: string;
  systemPrompt?: string;
  appendSystemPrompt?: string;
  sessionName?: string;
  maxSteps?: number;
  maxBudgetUsd?: number;
  planningMode?: "immediate" | "plan_first";
  permissionMode?: "default" | "auto" | "strict" | "bypass";
  /** Comma-separated allow-list → `--allow-tools`. */
  allowedTools?: string[];
  canUseTool?: CanUseTool;
  /** Local hook callbacks keyed by id (sent via `initialize`). */
  hookCallbacks?: Map<string, HookCallback>;
  /** Wire shape for `initialize.hooks` (event → matchers with callback ids). */
  initializeHooks?: Record<string, Array<{ hookCallbackIds: string[] }>>;
  abortSignal?: AbortSignal;
  /** When true, do not close stdin after the first result (multi-turn). */
  keepStdinOpen?: boolean;
}

export interface ControlSessionHandle {
  /** Async iterator of parsed wire items (SDK messages + results). */
  items(): AsyncGenerator<WireItem>;
  /** Send a follow-up user turn (`type:user` on stdin). */
  writeUser(text: string): void;
  /** Host→CLI `interrupt`. */
  interrupt(): Promise<void>;
  /** Host→CLI `set_permission_mode`. */
  setPermissionMode(mode: string): Promise<void>;
  /** Host→CLI `set_model`. */
  setModel(model: string): Promise<void>;
  /** Close stdin (ends multi-turn wait) and tear down. */
  close(): void;
  cancel(): void;
  getSessionId(): string | undefined;
}

/**
 * Build argv for a bidirectional control session (no `-H`).
 */
export function buildControlCliArgs(options: ControlSpawnOptions): string[] {
  const args: string[] = [
    "-p",
    options.prompt,
    "--output-format",
    "stream-json",
    "--input-format",
    "stream-json",
    "--permission-mode",
    mapPermissionMode(options.permissionMode, options.planningMode),
  ];

  if (options.resumeSessionId) {
    args.push("-r", options.resumeSessionId);
  }
  if (options.cwd) {
    args.push("--workspace", options.cwd);
  }
  if (options.systemPrompt) {
    args.push("--system-prompt", options.systemPrompt);
  }
  if (options.appendSystemPrompt) {
    args.push("--append-system-prompt", options.appendSystemPrompt);
  }
  if (options.sessionName) {
    args.push("--name", options.sessionName);
  }
  if (options.maxSteps != null) {
    args.push("--max-steps", String(options.maxSteps));
  }
  if (options.maxBudgetUsd != null) {
    args.push("--max-budget-usd", String(options.maxBudgetUsd));
  }
  if (options.model) {
    args.push("-m", options.model);
  }
  if (options.allowedTools && options.allowedTools.length > 0) {
    args.push("--allow-tools", options.allowedTools.join(","));
  }

  return args;
}

function mapPermissionMode(
  mode: ControlSpawnOptions["permissionMode"] | undefined,
  planning: ControlSpawnOptions["planningMode"] | undefined,
): string {
  if (planning === "plan_first") return "plan";
  switch (mode) {
    case "auto":
    case "bypass":
      return "auto";
    case "strict":
      return "strict";
    case "default":
    case undefined:
      return "default";
    default:
      return "default";
  }
}

/**
 * When the host did not pass `canUseTool`, decide whether to auto-allow.
 * Without a callback the CLI would hang waiting on stdin — so we allow by
 * default, optionally filtered by `allowedTools`.
 */
function shouldAutoAllow(
  allowedTools: string[] | undefined,
  toolName: string,
): boolean {
  if (!allowedTools || allowedTools.length === 0) return true;
  return allowedTools.includes(toolName);
}

/**
 * Spawn `recursive` with stdin open for control + streaming-input.
 */
export function spawnControlSession(
  options: ControlSpawnOptions,
): ControlSessionHandle {
  const bin = findRecursiveBinary(options.cliPath);
  const args = buildControlCliArgs(options);
  const cwd = options.cwd ?? process.cwd();

  let child: ChildProcess;
  try {
    child = spawn(bin, args, {
      cwd,
      stdio: ["pipe", "pipe", "pipe"],
      env: process.env,
    });
  } catch (err) {
    throw new RecursiveAgentError(
      `failed to spawn recursive CLI (${bin}): ${err}`,
      { isRetryable: false },
    );
  }

  if (!child.stdin || !child.stdout || !child.stderr) {
    child.kill();
    throw new RecursiveAgentError("failed to open CLI stdio pipes");
  }

  const stdin = child.stdin;
  const stdout = child.stdout;
  const stderr = child.stderr;

  let sessionId: string | undefined = options.resumeSessionId;
  let killed = false;
  let stdinClosed = false;
  const stderrChunks: string[] = [];
  const pendingHost = new Map<
    string,
    { resolve: (v: Record<string, unknown>) => void; reject: (e: Error) => void }
  >();

  stderr.setEncoding("utf8");
  stderr.on("data", (chunk: string) => {
    stderrChunks.push(chunk);
    if (stderrChunks.length > 200) stderrChunks.shift();
  });

  const writeLine = (obj: Record<string, unknown>): void => {
    if (stdinClosed || !stdin.writable) return;
    stdin.write(`${JSON.stringify(obj)}\n`);
  };

  const sendControlRequest = (
    request: Record<string, unknown>,
  ): Promise<Record<string, unknown>> => {
    const requestId = randomUUID();
    return new Promise((resolve, reject) => {
      pendingHost.set(requestId, { resolve, reject });
      writeLine({
        type: "control_request",
        request_id: requestId,
        request,
      });
    });
  };

  const replyControl = (
    requestId: string,
    response: Record<string, unknown>,
  ): void => {
    writeLine({
      type: "control_response",
      response: {
        subtype: "success",
        request_id: requestId,
        response,
      },
    });
  };

  const handleControlRequest = async (
    requestId: string,
    request: Record<string, unknown>,
  ): Promise<void> => {
    const subtype = String(request["subtype"] ?? "");
    if (subtype === "can_use_tool") {
      const toolName = String(request["tool_name"] ?? "");
      const input = (request["input"] as Record<string, unknown>) ?? {};
      const toolUseId =
        typeof request["tool_use_id"] === "string"
          ? request["tool_use_id"]
          : undefined;

      let decision: PermissionResult;
      if (options.canUseTool) {
        decision = await options.canUseTool(toolName, input, {
          toolUseId,
          signal: options.abortSignal,
        });
      } else if (shouldAutoAllow(options.allowedTools, toolName)) {
        decision = { behavior: "allow" };
      } else {
        decision = {
          behavior: "deny",
          message: `tool '${toolName}' not allowed (pass canUseTool or allowedTools)`,
        };
      }
      replyControl(requestId, decision as unknown as Record<string, unknown>);
      return;
    }

    if (subtype === "hook_callback") {
      const callbackId = String(request["callback_id"] ?? "");
      const input = (request["input"] as Record<string, unknown>) ?? {};
      const toolUseId =
        typeof request["tool_use_id"] === "string"
          ? request["tool_use_id"]
          : undefined;
      const cb = options.hookCallbacks?.get(callbackId);
      let result: Record<string, unknown> = {};
      if (cb) {
        const out = await cb(input, toolUseId, {
          signal: options.abortSignal,
        });
        if (out && typeof out === "object") result = out;
      }
      replyControl(requestId, result);
      return;
    }

    // Unknown CLI→host request: acknowledge empty success so the CLI does not hang.
    replyControl(requestId, {});
  };

  // Send initialize early so hooks are registered before the first tool call.
  // Do not await the reply here — the stdout demux in items() resolves it.
  if (options.initializeHooks && Object.keys(options.initializeHooks).length > 0) {
    const requestId = randomUUID();
    pendingHost.set(requestId, {
      resolve: () => undefined,
      reject: () => undefined,
    });
    writeLine({
      type: "control_request",
      request_id: requestId,
      request: {
        subtype: "initialize",
        hooks: options.initializeHooks,
      },
    });
  }

  if (options.abortSignal) {
    if (options.abortSignal.aborted) {
      child.kill("SIGTERM");
    } else {
      options.abortSignal.addEventListener(
        "abort",
        () => {
          void sendControlRequest({ subtype: "interrupt" }).finally(() => {
            child.kill("SIGTERM");
          });
        },
        { once: true },
      );
    }
  }

  const closeStdin = (): void => {
    if (stdinClosed) return;
    stdinClosed = true;
    try {
      stdin.end();
    } catch {
      /* ignore */
    }
  };

  return {
    cancel() {
      if (!killed && child.exitCode === null) {
        killed = true;
        child.kill("SIGTERM");
      }
      closeStdin();
    },
    close() {
      closeStdin();
    },
    getSessionId() {
      return sessionId;
    },
    writeUser(text: string) {
      writeLine({
        type: "user",
        message: { role: "user", content: text },
      });
    },
    async interrupt() {
      try {
        await sendControlRequest({ subtype: "interrupt" });
      } catch {
        /* fall through to SIGTERM */
      }
      this.cancel();
    },
    async setPermissionMode(mode: string) {
      await sendControlRequest({ subtype: "set_permission_mode", mode });
    },
    async setModel(model: string) {
      await sendControlRequest({ subtype: "set_model", model });
    },
    async *items(): AsyncGenerator<WireItem> {
      const rl = createInterface({ input: stdout, crlfDelay: Infinity });
      let sawResult = false;
      try {
        for await (const line of rl) {
          const trimmed = line.trim();
          if (!trimmed) continue;
          let raw: Record<string, unknown>;
          try {
            raw = JSON.parse(trimmed) as Record<string, unknown>;
          } catch {
            continue;
          }

          const type = String(raw["type"] ?? "");
          if (type === "control_request") {
            const requestId = String(raw["request_id"] ?? "");
            const request =
              (raw["request"] as Record<string, unknown>) ?? {};
            // Handle asynchronously so the read loop stays live for more lines.
            void handleControlRequest(requestId, request);
            continue;
          }
          if (type === "control_response") {
            const response = raw["response"] as
              | Record<string, unknown>
              | undefined;
            const requestId = String(
              response?.["request_id"] ?? raw["request_id"] ?? "",
            );
            const pending = pendingHost.get(requestId);
            if (pending) {
              pendingHost.delete(requestId);
              pending.resolve(
                (response?.["response"] as Record<string, unknown>) ??
                  response ??
                  {},
              );
            }
            continue;
          }

          const item = parseWireObject(raw, sessionId ?? "");
          if (!item) continue;
          if (item.kind === "session") {
            sessionId = item.sessionId;
            if (item.message) {
              yield { kind: "message", message: item.message };
            }
            continue;
          }
          if (item.kind === "message" && item.message.type === "assistant") {
            item.message.sessionId = sessionId ?? item.message.sessionId;
          }
          if (item.kind === "result") {
            sawResult = true;
            // Single-shot: after the first turn result, close stdin so the CLI
            // exits the streaming-input wait loop. Multi-turn hosts that call
            // writeUser() should keep the session open via streamInput instead.
            if (!options.keepStdinOpen) {
              closeStdin();
            }
          }
          yield item;
        }
      } finally {
        rl.close();
        closeStdin();
        for (const [, p] of pendingHost) {
          p.reject(new Error("CLI stdout closed before control response"));
        }
        pendingHost.clear();
      }

      const exitCode: number | null = await new Promise((resolve) => {
        if (child.exitCode !== null) {
          resolve(child.exitCode);
          return;
        }
        child.once("exit", (code) => resolve(code));
      });

      if (sawResult) return;

      if (killed) {
        yield {
          kind: "result",
          result: {
            id: sessionId ?? "",
            status: "cancelled",
            subtype: "cancelled",
            ok: false,
            error: "cancelled",
          },
        };
        return;
      }

      const errTail = stderrChunks.join("").trim().slice(-500);
      yield {
        kind: "result",
        result: {
          id: sessionId ?? "",
          status: "error",
          subtype: "error_during_execution",
          ok: false,
          error:
            errTail ||
            `recursive CLI exited with code ${exitCode ?? "unknown"} without a result`,
        },
      };
    },
  };
}
