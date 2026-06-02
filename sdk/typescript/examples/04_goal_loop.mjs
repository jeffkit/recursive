/**
 * 04-goal-loop — use the autonomous goal loop (g168) to drive a session
 *                until a natural-language condition is met.
 *
 * Endpoints:
 *   POST   /sessions/{id}/goal   { condition, max_turns }
 *   DELETE /sessions/{id}/goal
 *   GET    /sessions/{id}        (returns `goal: GoalState | null`)
 */

import { Agent, RecursiveClient } from "../dist/index.mjs";

const baseUrl = process.env.RECURSIVE_BASE_URL ?? "http://127.0.0.1:3000";

const agent = await Agent.create({ baseUrl });
const client = new RecursiveClient({ baseUrl });

try {
  await client.setGoal(
    agent.sessionId,
    "all unit tests in this repo pass",
    { maxTurns: 10 },
  );
  console.log("goal armed.");

  // Kick off the loop with an initial nudge.
  const run = await agent.send("Get started on the goal.");
  await run.wait().catch(() => {});

  // Poll until the loop terminates or maxTurns is reached.
  while (true) {
    const goal = await client.getGoal(agent.sessionId);
    if (!goal) {
      console.log("goal cleared.");
      break;
    }
    console.log(
      `[${goal.turns}/${goal.maxTurns}] ${goal.status}` +
        (goal.lastReason ? `  — ${goal.lastReason}` : ""),
    );
    if (goal.status !== "pursuing") break;
    await new Promise((r) => setTimeout(r, 1500));
  }
} finally {
  await client.clearGoal(agent.sessionId).catch(() => {});
  await agent.close();
}
