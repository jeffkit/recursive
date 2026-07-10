/**
 * Spawn `recursive -p … --output-format stream-json` and stream NDJSON.
 */

import { spawn, type ChildProcess } from "node:child_process";
import { createInterface } from "node:readline";

import { findRecursiveBinary } from "./binary.js";
import { RecursiveAgentError } from "./exceptions.js";
import { parseWireObject, type WireItem } from "./wire.js";

export interface CliSpawnOptions {
  /** Prompt for this turn (`-p`). */
  prompt: string;
  /** Resume an existing session (`-r`). */
  resumeSessionId?: string;
  /** Working directory / workspace root. */
  cwd?: string;
  /** Path to the `recursive` binary. */
  cliPath?: string;
  /** Model id (`-m`). */
  model?: string;
  systemPrompt?: string;
  appendSystemPrompt?: string;
  sessionName?: string;
  maxSteps?: number;
  maxBudgetUsd?: number;
  planningMode?: "immediate" | "plan_first";
  permissionMode?: "default" | "auto" | "strict" | "bypass";
}

/**
 * Build CLI argv for a one-shot / resume turn.
 *
 * Always uses Claude-compatible `stream-json` so the SDK can stream and
 * collect a terminal `result` envelope.
 */
export function buildCliArgs(options: CliSpawnOptions): string[] {
  const args: string[] = [
    "-p",
    options.prompt,
    "--output-format",
    "stream-json",
    // Non-interactive: auto-approve tools (SDK hosts have no TTY prompt).
    "--permission-mode",
    mapPermissionMode(options.permissionMode, options.planningMode),
    "-H",
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

  return args;
}

function mapPermissionMode(
  mode: CliSpawnOptions["permissionMode"] | undefined,
  planning: CliSpawnOptions["planningMode"] | undefined,
): string {
  if (planning === "plan_first") return "plan";
  switch (mode) {
    case "auto":
    case "bypass":
      return "auto";
    case "strict":
    case "default":
    case undefined:
      return "default";
    default:
      return "default";
  }
}

export interface CliProcessHandle {
  /** Kill the child (best-effort cancel). */
  cancel(): void;
  /** Async iterator of parsed wire items until the process exits. */
  items(): AsyncGenerator<WireItem>;
  /** Captured session id from `system/init`, if seen. */
  getSessionId(): string | undefined;
}

/**
 * Spawn the recursive CLI and return a handle that yields parsed wire items.
 */
export function spawnCliProcess(options: CliSpawnOptions): CliProcessHandle {
  const bin = findRecursiveBinary(options.cliPath);
  const args = buildCliArgs(options);
  const cwd = options.cwd ?? process.cwd();

  let child: ChildProcess;
  try {
    child = spawn(bin, args, {
      cwd,
      stdio: ["ignore", "pipe", "pipe"],
      env: process.env,
    });
  } catch (err) {
    throw new RecursiveAgentError(
      `failed to spawn recursive CLI (${bin}): ${err}`,
      { isRetryable: false },
    );
  }

  if (!child.stdout || !child.stderr) {
    child.kill();
    throw new RecursiveAgentError("failed to open CLI stdio pipes");
  }

  const stdout = child.stdout;
  const stderr = child.stderr;

  let sessionId: string | undefined = options.resumeSessionId;
  let killed = false;
  const stderrChunks: string[] = [];

  stderr.setEncoding("utf8");
  stderr.on("data", (chunk: string) => {
    stderrChunks.push(chunk);
    // Cap stderr buffer so a noisy CLI cannot OOM the host.
    if (stderrChunks.length > 200) stderrChunks.shift();
  });

  return {
    cancel() {
      if (!killed && child.exitCode === null) {
        killed = true;
        child.kill("SIGTERM");
      }
    },
    getSessionId() {
      return sessionId;
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
          if (item.kind === "result") sawResult = true;
          yield item;
        }
      } finally {
        rl.close();
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

      // CLI exited without a result envelope — synthesise an error.
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
