import { describe, it, expect, vi, beforeEach } from "vitest";
import { RecursiveClient } from "../src/client.js";
import { HttpClient } from "../src/http.js";

beforeEach(() => vi.restoreAllMocks());

describe("RecursiveClient.listTools", () => {
  it("maps tool fields", async () => {
    vi.spyOn(HttpClient.prototype, "get").mockResolvedValue([
      { name: "read", description: "read file", parameters: { p: 1 } },
    ]);

    const client = new RecursiveClient({ baseUrl: "http://localhost:3000" });
    const tools = await client.listTools();

    expect(tools).toHaveLength(1);
    expect(tools[0]!.name).toBe("read");
    expect(tools[0]!.parameters).toEqual({ p: 1 });
  });
});

describe("RecursiveClient.getSession", () => {
  it("parses Plan Mode 2.0 / goal / todos fields", async () => {
    vi.spyOn(HttpClient.prototype, "get").mockResolvedValue({
      id: "s1",
      created_at: "2026-06-01",
      messages: [],
      status: "plan_pending_approval",
      pending_plan: "draft plan…",
      todos: [{ subject: "x" }],
      goal: {
        condition: "tests pass",
        status: "pursuing",
        turns: 3,
        max_turns: 20,
        last_reason: "running",
      },
    });

    const client = new RecursiveClient({ baseUrl: "http://localhost:3000" });
    const detail = await client.getSession("s1");

    expect(detail.status).toBe("plan_pending_approval");
    expect(detail.pendingPlan).toBe("draft plan…");
    expect(detail.todos).toHaveLength(1);
    expect(detail.goal?.condition).toBe("tests pass");
    expect(detail.goal?.maxTurns).toBe(20);
    expect(detail.goal?.lastReason).toBe("running");
  });

  it("returns goal=undefined when not set", async () => {
    vi.spyOn(HttpClient.prototype, "get").mockResolvedValue({
      id: "s1",
      created_at: "",
      messages: [],
      status: "idle",
    });

    const client = new RecursiveClient({ baseUrl: "http://localhost:3000" });
    const detail = await client.getSession("s1");

    expect(detail.goal).toBeUndefined();
    expect(detail.pendingPlan).toBeUndefined();
  });
});

describe("RecursiveClient plan approval", () => {
  it("approvePlan posts edits when given", async () => {
    const spy = vi
      .spyOn(HttpClient.prototype, "post")
      .mockResolvedValue({ status: "approved", session_id: "s1" });

    const client = new RecursiveClient({ baseUrl: "http://localhost:3000" });
    const resp = await client.approvePlan("s1", { edits: "tweaked" });

    expect(resp.status).toBe("approved");
    expect(resp.sessionId).toBe("s1");
    expect(spy).toHaveBeenCalledWith("/sessions/s1/plan/confirm", {
      edits: "tweaked",
    });
  });

  it("approvePlan omits edits when not given", async () => {
    const spy = vi
      .spyOn(HttpClient.prototype, "post")
      .mockResolvedValue({ status: "approved", session_id: "s1" });

    const client = new RecursiveClient({ baseUrl: "http://localhost:3000" });
    await client.approvePlan("s1");

    expect(spy).toHaveBeenCalledWith("/sessions/s1/plan/confirm", {});
  });

  it("rejectPlan posts reason (default empty)", async () => {
    const spy = vi
      .spyOn(HttpClient.prototype, "post")
      .mockResolvedValue({ status: "rejected", session_id: "s1" });

    const client = new RecursiveClient({ baseUrl: "http://localhost:3000" });
    const resp = await client.rejectPlan("s1", { reason: "too risky" });

    expect(resp.status).toBe("rejected");
    expect(spy).toHaveBeenCalledWith("/sessions/s1/plan/reject", {
      reason: "too risky",
    });
  });
});

describe("RecursiveClient goal loop", () => {
  it("setGoal sends condition and max_turns", async () => {
    const spy = vi
      .spyOn(HttpClient.prototype, "post")
      .mockResolvedValue({ status: "pursuing", session_id: "s1" });

    const client = new RecursiveClient({ baseUrl: "http://localhost:3000" });
    const resp = await client.setGoal("s1", "all tests pass", { maxTurns: 30 });

    expect(resp.status).toBe("pursuing");
    expect(spy).toHaveBeenCalledWith("/sessions/s1/goal", {
      condition: "all tests pass",
      max_turns: 30,
    });
  });

  it("setGoal defaults maxTurns to 20", async () => {
    const spy = vi
      .spyOn(HttpClient.prototype, "post")
      .mockResolvedValue({ status: "pursuing", session_id: "s1" });

    const client = new RecursiveClient({ baseUrl: "http://localhost:3000" });
    await client.setGoal("s1", "done");

    const body = spy.mock.calls[0]![1] as Record<string, unknown>;
    expect(body["max_turns"]).toBe(20);
  });

  it("getGoal returns null when no goal", async () => {
    vi.spyOn(HttpClient.prototype, "get").mockResolvedValue({
      id: "s1",
      created_at: "",
      messages: [],
      status: "idle",
    });

    const client = new RecursiveClient({ baseUrl: "http://localhost:3000" });
    const goal = await client.getGoal("s1");
    expect(goal).toBeNull();
  });

  it("getGoal returns parsed GoalState", async () => {
    vi.spyOn(HttpClient.prototype, "get").mockResolvedValue({
      id: "s1",
      created_at: "",
      messages: [],
      status: "idle",
      goal: {
        condition: "x",
        status: "pursuing",
        turns: 1,
        max_turns: 10,
      },
    });

    const client = new RecursiveClient({ baseUrl: "http://localhost:3000" });
    const goal = await client.getGoal("s1");
    expect(goal?.condition).toBe("x");
    expect(goal?.turns).toBe(1);
    expect(goal?.maxTurns).toBe(10);
  });
});

describe("RecursiveClient.listSlashCommands", () => {
  it("maps fields including aliases and argument_hint", async () => {
    vi.spyOn(HttpClient.prototype, "get").mockResolvedValue([
      {
        name: "commit",
        description: "make a commit",
        source: "builtin",
        aliases: ["c"],
        argument_hint: "[message]",
      },
      {
        name: "review",
        description: "skill cmd",
        source: "skill",
      },
    ]);

    const client = new RecursiveClient({ baseUrl: "http://localhost:3000" });
    const cmds = await client.listSlashCommands();

    expect(cmds).toHaveLength(2);
    expect(cmds[0]!.aliases).toEqual(["c"]);
    expect(cmds[0]!.argumentHint).toBe("[message]");
    expect(cmds[1]!.aliases).toEqual([]);
    expect(cmds[1]!.argumentHint).toBe("");
  });
});

describe("RecursiveClient.deleteSession", () => {
  it("calls DELETE /sessions/{id}", async () => {
    const spy = vi
      .spyOn(HttpClient.prototype, "delete")
      .mockResolvedValue(undefined);

    const client = new RecursiveClient({ baseUrl: "http://localhost:3000" });
    await client.deleteSession("s1");
    expect(spy).toHaveBeenCalledWith("/sessions/s1");
  });
});
