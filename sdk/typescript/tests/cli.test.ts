/**
 * Unit tests for CLI wire parsing and argv building.
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { buildCliArgs } from "../src/subprocess.js";
import { parseWireObject } from "../src/wire.js";
import { findRecursiveBinary } from "../src/binary.js";
import { RecursiveAgentError } from "../src/exceptions.js";
import { Agent, Run } from "../src/index.js";
import type { CliProcessHandle } from "../src/subprocess.js";
import type { WireItem } from "../src/wire.js";

describe("buildCliArgs", () => {
  it("includes stream-json, headless, and permission-mode", () => {
    const args = buildCliArgs({ prompt: "hello" });
    expect(args).toContain("-p");
    expect(args).toContain("hello");
    expect(args).toContain("--output-format");
    expect(args).toContain("stream-json");
    expect(args).toContain("-H");
    expect(args).toContain("--permission-mode");
    expect(args).toContain("default");
  });

  it("maps bypass permission to auto", () => {
    const args = buildCliArgs({ prompt: "x", permissionMode: "bypass" });
    const idx = args.indexOf("--permission-mode");
    expect(args[idx + 1]).toBe("auto");
  });

  it("maps plan_first to plan permission mode", () => {
    const args = buildCliArgs({ prompt: "x", planningMode: "plan_first" });
    const idx = args.indexOf("--permission-mode");
    expect(args[idx + 1]).toBe("plan");
  });

  it("adds resume and workspace flags", () => {
    const args = buildCliArgs({
      prompt: "cont",
      resumeSessionId: "sess-1",
      cwd: "/tmp/ws",
      model: "deepseek-chat",
      maxSteps: 5,
    });
    expect(args).toContain("-r");
    expect(args).toContain("sess-1");
    expect(args).toContain("--workspace");
    expect(args).toContain("/tmp/ws");
    expect(args).toContain("-m");
    expect(args).toContain("deepseek-chat");
    expect(args).toContain("--max-steps");
    expect(args).toContain("5");
  });
});

describe("parseWireObject", () => {
  it("parses system/init as session + message", () => {
    const item = parseWireObject(
      {
        type: "system",
        subtype: "init",
        session_id: "abc",
        model: "m",
      },
      "",
    );
    expect(item?.kind).toBe("session");
    if (item?.kind === "session") {
      expect(item.sessionId).toBe("abc");
      expect(item.message?.type).toBe("system");
    }
  });

  it("parses assistant text content", () => {
    const item = parseWireObject(
      {
        type: "assistant",
        session_id: "s1",
        message: {
          content: [{ type: "text", text: "hi" }],
        },
      },
      "s1",
    );
    expect(item?.kind).toBe("message");
    if (item?.kind === "message" && item.message.type === "assistant") {
      expect(item.message.content).toEqual([{ type: "text", text: "hi" }]);
    }
  });

  it("parses stream_event text delta", () => {
    const item = parseWireObject(
      {
        type: "stream_event",
        session_id: "s1",
        event: {
          type: "content_block_delta",
          index: 0,
          delta: { type: "text_delta", text: "tok" },
        },
      },
      "s1",
    );
    expect(item?.kind).toBe("message");
    if (item?.kind === "message" && item.message.type === "stream_event") {
      expect(item.message.text).toBe("tok");
    }
  });

  it("parses success result", () => {
    const item = parseWireObject(
      {
        type: "result",
        subtype: "success",
        is_error: false,
        session_id: "s1",
        result: "done",
        num_turns: 2,
        duration_ms: 100,
        usage: { input_tokens: 10, output_tokens: 5 },
      },
      "s1",
    );
    expect(item?.kind).toBe("result");
    if (item?.kind === "result") {
      expect(item.result.ok).toBe(true);
      expect(item.result.result).toBe("done");
      expect(item.result.usage?.inputTokens).toBe(10);
      expect(item.result.numTurns).toBe(2);
    }
  });

  it("parses error result", () => {
    const item = parseWireObject(
      {
        type: "result",
        subtype: "error_max_turns",
        is_error: true,
        session_id: "s1",
        errors: ["budget"],
      },
      "s1",
    );
    expect(item?.kind).toBe("result");
    if (item?.kind === "result") {
      expect(item.result.status).toBe("error");
      expect(item.result.subtype).toBe("error_max_turns");
      expect(item.result.error).toBe("budget");
    }
  });
});

describe("findRecursiveBinary", () => {
  const origPath = process.env["PATH"];
  const origBin = process.env["RECURSIVE_BIN"];

  afterEach(() => {
    if (origPath === undefined) delete process.env["PATH"];
    else process.env["PATH"] = origPath;
    if (origBin === undefined) delete process.env["RECURSIVE_BIN"];
    else process.env["RECURSIVE_BIN"] = origBin;
  });

  it("throws when binary is missing", () => {
    delete process.env["RECURSIVE_BIN"];
    process.env["PATH"] = "/nonexistent-dir-for-sdk-test";
    expect(() => findRecursiveBinary()).toThrow(RecursiveAgentError);
  });
});

describe("Run from CLI handle", () => {
  it("streams messages and returns result from wait()", async () => {
    const items: WireItem[] = [
      {
        kind: "session",
        sessionId: "sess-cli",
        message: {
          type: "system",
          subtype: "init",
          data: { session_id: "sess-cli" },
        },
      },
      {
        kind: "message",
        message: {
          type: "assistant",
          content: [{ type: "text", text: "hello" }],
          sessionId: "sess-cli",
        },
      },
      {
        kind: "result",
        result: {
          id: "sess-cli",
          status: "finished",
          subtype: "success",
          ok: true,
          result: "hello",
        },
      },
    ];

    const handle: CliProcessHandle = {
      cancel: vi.fn(),
      getSessionId: () => "sess-cli",
      async *items() {
        for (const item of items) {
          if (item.kind === "session" && item.message) {
            yield { kind: "message", message: item.message };
          } else if (item.kind !== "session") {
            yield item;
          }
        }
      },
    };

    let captured = "";
    const run = Run._fromCli("", handle, (id) => {
      captured = id;
    });

    const texts: string[] = [];
    for await (const msg of run.stream()) {
      if (msg.type === "assistant") {
        for (const b of msg.content) {
          if (b.type === "text") texts.push(b.text);
        }
      }
    }
    const result = await run.wait();
    expect(texts).toEqual(["hello"]);
    expect(result.ok).toBe(true);
    expect(result.result).toBe("hello");
    expect(captured).toBe("sess-cli");
    expect(run.id).toBe("sess-cli");
  });
});

describe("Agent.create CLI mode", () => {
  beforeEach(() => {
    delete process.env["RECURSIVE_BASE_URL"];
  });

  it("creates a CLI session without HTTP when baseUrl is omitted", async () => {
    const agent = await Agent.create({ permissionMode: "auto" });
    expect(agent.sessionId).toBe("");
    await agent.close();
  });

  it("listSessions requires HTTP", async () => {
    await expect(Agent.listSessions()).rejects.toThrow(/HTTP transport/);
  });
});
