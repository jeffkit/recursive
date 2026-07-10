/**
 * Tests for Claude-compatible `query()` API and control-session argv.
 */

import { describe, it, expect, vi } from "vitest";
import { optionsToSpawn, query } from "../src/query.js";
import { buildControlCliArgs } from "../src/controlSession.js";
import type { ControlSessionHandle } from "../src/controlSession.js";
import type { WireItem } from "../src/wire.js";
import * as controlSession from "../src/controlSession.js";

describe("optionsToSpawn", () => {
  it("maps Claude option names onto control spawn opts", () => {
    const spawn = optionsToSpawn("fix auth", {
      cwd: "/tmp/ws",
      model: "deepseek-chat",
      maxTurns: 7,
      permissionMode: "bypassPermissions",
      resume: "sess-9",
      pathToClaudeCodeExecutable: "/usr/bin/recursive",
      systemPrompt: "be brief",
      maxBudgetUsd: 1.5,
      allowedTools: ["Read", "Write"],
    });
    expect(spawn.prompt).toBe("fix auth");
    expect(spawn.cwd).toBe("/tmp/ws");
    expect(spawn.model).toBe("deepseek-chat");
    expect(spawn.maxSteps).toBe(7);
    expect(spawn.permissionMode).toBe("auto");
    expect(spawn.resumeSessionId).toBe("sess-9");
    expect(spawn.cliPath).toBe("/usr/bin/recursive");
    expect(spawn.systemPrompt).toBe("be brief");
    expect(spawn.maxBudgetUsd).toBe(1.5);
    expect(spawn.allowedTools).toEqual(["Read", "Write"]);
  });

  it("maps plan permissionMode to plan_first", () => {
    const spawn = optionsToSpawn("x", { permissionMode: "plan" });
    expect(spawn.planningMode).toBe("plan_first");
  });

  it("maps preset systemPrompt.append to appendSystemPrompt", () => {
    const spawn = optionsToSpawn("x", {
      systemPrompt: {
        type: "preset",
        preset: "claude_code",
        append: "\nAlways run tests.",
      },
    });
    expect(spawn.systemPrompt).toBeUndefined();
    expect(spawn.appendSystemPrompt).toBe("\nAlways run tests.");
  });
});

describe("buildControlCliArgs", () => {
  it("uses stream-json input/output and omits -H", () => {
    const args = buildControlCliArgs({ prompt: "hello" });
    expect(args).toContain("-p");
    expect(args).toContain("hello");
    expect(args).toContain("--output-format");
    expect(args).toContain("stream-json");
    expect(args).toContain("--input-format");
    expect(args).not.toContain("-H");
  });

  it("passes allowedTools as --allow-tools", () => {
    const args = buildControlCliArgs({
      prompt: "x",
      allowedTools: ["Read", "Bash"],
    });
    expect(args).toContain("--allow-tools");
    expect(args).toContain("Read,Bash");
  });
});

describe("query()", () => {
  it("yields assistant and result messages in the stream", async () => {
    const items: WireItem[] = [
      {
        kind: "message",
        message: {
          type: "system",
          subtype: "init",
          data: { session_id: "s1", type: "system", subtype: "init" },
        },
      },
      {
        kind: "message",
        message: {
          type: "assistant",
          content: [{ type: "text", text: "hello" }],
          sessionId: "s1",
        },
      },
      {
        kind: "result",
        result: {
          id: "s1",
          status: "finished",
          subtype: "success",
          ok: true,
          result: "hello",
          numTurns: 1,
        },
      },
    ];

    const handle: ControlSessionHandle = {
      cancel: vi.fn(),
      close: vi.fn(),
      getSessionId: () => "s1",
      writeUser: vi.fn(),
      interrupt: vi.fn(async () => undefined),
      setPermissionMode: vi.fn(async () => undefined),
      setModel: vi.fn(async () => undefined),
      async *items() {
        for (const item of items) yield item;
      },
    };

    vi.spyOn(controlSession, "spawnControlSession").mockReturnValue(handle);

    const messages = [];
    for await (const msg of query({ prompt: "hi", options: { maxTurns: 3 } })) {
      messages.push(msg);
    }

    expect(messages.some((m) => m.type === "assistant")).toBe(true);
    const result = messages.find((m) => m.type === "result");
    expect(result).toBeDefined();
    expect(result!["result"]).toBe("hello");
    expect(result!["subtype"]).toBe("success");
    expect(result!["is_error"]).toBe(false);

    vi.restoreAllMocks();
  });

  it("interrupt() cancels via control session", async () => {
    const interrupt = vi.fn(async () => undefined);
    const handle: ControlSessionHandle = {
      cancel: vi.fn(),
      close: vi.fn(),
      getSessionId: () => undefined,
      writeUser: vi.fn(),
      interrupt,
      setPermissionMode: vi.fn(async () => undefined),
      setModel: vi.fn(async () => undefined),
      async *items() {
        // never yields — caller interrupts
      },
    };
    vi.spyOn(controlSession, "spawnControlSession").mockReturnValue(handle);

    const q = query({ prompt: "x" });
    await q.interrupt();
    expect(interrupt).toHaveBeenCalled();
    q.close();

    vi.restoreAllMocks();
  });

  it("streamInput writes follow-up user turns", async () => {
    const writeUser = vi.fn();
    const close = vi.fn();
    const handle: ControlSessionHandle = {
      cancel: vi.fn(),
      close,
      getSessionId: () => "s1",
      writeUser,
      interrupt: vi.fn(async () => undefined),
      setPermissionMode: vi.fn(async () => undefined),
      setModel: vi.fn(async () => undefined),
      async *items() {
        yield {
          kind: "result",
          result: {
            id: "s1",
            status: "finished",
            subtype: "success",
            ok: true,
            result: "ok",
          },
        };
      },
    };
    vi.spyOn(controlSession, "spawnControlSession").mockReturnValue(handle);

    const q = query({ prompt: "first" });
    await q.streamInput(
      (async function* () {
        yield "second";
        yield "third";
      })(),
    );
    expect(writeUser).toHaveBeenCalledWith("second");
    expect(writeUser).toHaveBeenCalledWith("third");
    expect(close).toHaveBeenCalled();

    // drain generator
    for await (const _ of q) {
      /* ignore */
    }

    vi.restoreAllMocks();
  });
});
