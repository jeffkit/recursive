import { describe, it, expect, vi, beforeEach } from "vitest";
import { Agent, RecursiveAgentError } from "../src/index.js";
import { HttpClient } from "../src/http.js";
import { Run } from "../src/run.js";

// ── helpers ────────────────────────────────────────────────────────────────

function mockGet(result: unknown) {
  return vi.spyOn(HttpClient.prototype, "get").mockResolvedValue(result);
}

function mockPost(result: unknown) {
  return vi.spyOn(HttpClient.prototype, "post").mockResolvedValue(result);
}

function mockDelete() {
  return vi.spyOn(HttpClient.prototype, "delete").mockResolvedValue(undefined);
}

async function* fakeStream(
  events: Array<{ type: string; data: unknown }>,
) {
  for (const ev of events) {
    yield ev;
  }
}

// ── RecursiveAgentError ───────────────────────────────────────────────────

describe("RecursiveAgentError", () => {
  it("has message and isRetryable", () => {
    const err = new RecursiveAgentError("bad auth", { isRetryable: false });
    expect(err.message).toBe("bad auth");
    expect(err.isRetryable).toBe(false);
    expect(err instanceof RecursiveAgentError).toBe(true);
    expect(err instanceof Error).toBe(true);
  });

  it("defaults isRetryable to false", () => {
    const err = new RecursiveAgentError("oops");
    expect(err.isRetryable).toBe(false);
  });
});

// ── Agent.prompt ──────────────────────────────────────────────────────────

describe("Agent.prompt", () => {
  beforeEach(() => vi.restoreAllMocks());

  it("calls /run and returns RunResult", async () => {
    const spy = mockPost({
      status: "finished",
      finish_reason: "stop",
      session_id: "s1",
    });

    const result = await Agent.prompt("do something", {
      baseUrl: "http://localhost:3000",
    });

    expect(result.status).toBe("finished");
    expect(result.id).toBe("s1");
    expect(result.ok).toBe(true);
    expect(spy).toHaveBeenCalledWith("/run", { goal: "do something" });
  });

  it("passes system_prompt and max_steps", async () => {
    const spy = mockPost({ status: "finished" });

    await Agent.prompt("task", {
      baseUrl: "http://localhost:3000",
      systemPrompt: "sys",
      maxSteps: 10,
    });

    const body = spy.mock.calls[0]![1] as Record<string, unknown>;
    expect(body["system_prompt"]).toBe("sys");
    expect(body["max_steps"]).toBe(10);
  });

  it("parses usage", async () => {
    mockPost({
      status: "finished",
      usage: { input_tokens: 100, output_tokens: 50 },
    });

    const result = await Agent.prompt("x", { baseUrl: "http://localhost:3000" });
    expect(result.usage?.inputTokens).toBe(100);
    expect(result.usage?.outputTokens).toBe(50);
  });
});

// ── Agent.create ──────────────────────────────────────────────────────────

describe("Agent.create", () => {
  beforeEach(() => vi.restoreAllMocks());

  it("creates session and returns AgentSession", async () => {
    mockPost({ id: "sess-1" });
    const del = mockDelete();

    const agent = await Agent.create({ baseUrl: "http://localhost:3000" });
    expect(agent.sessionId).toBe("sess-1");

    await agent.close();
    expect(del).toHaveBeenCalledWith("/sessions/sess-1");
  });

  it("sends message via agent.send", async () => {
    mockPost({ id: "sess-2" });
    mockDelete();

    const agent = await Agent.create({ baseUrl: "http://localhost:3000" });

    // stub send call
    const sendSpy = vi
      .spyOn(HttpClient.prototype, "post")
      .mockResolvedValueOnce({}) // send message
    ;

    const run = await agent.send("hello");
    expect(run).toBeInstanceOf(Run);

    await agent.close();
  });
});

// ── Agent.resume ─────────────────────────────────────────────────────────

describe("Agent.resume", () => {
  beforeEach(() => vi.restoreAllMocks());

  it("does not delete session on close", async () => {
    mockGet({ id: "old-sess" });
    const del = mockDelete();

    const agent = await Agent.resume("old-sess", {
      baseUrl: "http://localhost:3000",
    });
    await agent.close();

    expect(del).not.toHaveBeenCalled();
  });
});

// ── Run ───────────────────────────────────────────────────────────────────

describe("Run", () => {
  beforeEach(() => vi.restoreAllMocks());

  it("stream yields assistant messages", async () => {
    mockPost({ id: "sess-3" });
    mockDelete();

    vi.spyOn(HttpClient.prototype, "streamEvents").mockReturnValue(
      fakeStream([
        {
          type: "message",
          data: { role: "assistant", content: "Hello!" },
        },
        { type: "done", data: { status: "finished" } },
      ]) as unknown as AsyncGenerator<{ type: string; data: unknown }>,
    );

    const agent = await Agent.create({ baseUrl: "http://localhost:3000" });

    // stub send
    vi.spyOn(HttpClient.prototype, "post").mockResolvedValueOnce({});
    const run = await agent.send("hi");

    const messages = [];
    for await (const msg of run.stream()) {
      messages.push(msg);
    }

    expect(messages.length).toBe(1);
    const msg = messages[0];
    expect(msg!.type).toBe("assistant");

    await agent.close();
  });

  it("wait caches result — stream not called twice", async () => {
    mockPost({ id: "sess-4" });
    mockDelete();

    const streamSpy = vi
      .spyOn(HttpClient.prototype, "streamEvents")
      .mockReturnValue(
        fakeStream([
          { type: "done", data: { status: "finished", finish_reason: "stop" } },
        ]) as unknown as AsyncGenerator<{ type: string; data: unknown }>,
      );

    const agent = await Agent.create({ baseUrl: "http://localhost:3000" });
    vi.spyOn(HttpClient.prototype, "post").mockResolvedValueOnce({});
    const run = await agent.send("go");

    const r1 = await run.wait();
    const r2 = await run.wait(); // should return cached

    expect(r1).toBe(r2);
    expect(r1.finishReason).toBe("stop");
    // streamEvents called once for the single send
    expect(streamSpy).toHaveBeenCalledTimes(1);

    await agent.close();
  });

  it("supports() returns true for known ops", () => {
    const run = new Run("test", {} as unknown as HttpClient);
    expect(run.supports("stream")).toBe(true);
    expect(run.supports("wait")).toBe(true);
    expect(run.supports("unknown")).toBe(false);
  });
});

// ── Agent.listSessions ────────────────────────────────────────────────────

describe("Agent.listSessions", () => {
  beforeEach(() => vi.restoreAllMocks());

  it("returns session infos", async () => {
    mockGet([
      {
        id: "s1",
        created_at: "2026-06-01",
        message_count: 3,
        last_prompt: "hello",
      },
    ]);

    const sessions = await Agent.listSessions({
      baseUrl: "http://localhost:3000",
    });

    expect(sessions.length).toBe(1);
    expect(sessions[0]!.id).toBe("s1");
    expect(sessions[0]!.messageCount).toBe(3);
    expect(sessions[0]!.lastPrompt).toBe("hello");
  });
});

// ── Agent.getSessionMessages ───────────────────────────────────────────────

describe("Agent.getSessionMessages", () => {
  beforeEach(() => vi.restoreAllMocks());

  it("returns messages array from session detail", async () => {
    mockGet({
      id: "sess-42",
      messages: [
        { role: "user", content: "hello" },
        { role: "assistant", content: "hi there" },
      ],
    });

    const msgs = await Agent.getSessionMessages("sess-42", {
      baseUrl: "http://localhost:3000",
    });

    expect(msgs.length).toBe(2);
    expect(msgs[0]!["role"]).toBe("user");
    expect(msgs[1]!["content"]).toBe("hi there");
  });

  it("returns empty array when messages field is absent", async () => {
    mockGet({ id: "sess-99" });

    const msgs = await Agent.getSessionMessages("sess-99", {
      baseUrl: "http://localhost:3000",
    });

    expect(msgs).toEqual([]);
  });
});
