/**
 * 03-plan-mode — create a session, drive Plan Mode 2.0 (g165–167).
 *
 * Approve or reject the proposed plan with `RecursiveClient.approvePlan` /
 * `rejectPlan`. The server endpoints are:
 *
 *   POST /sessions/{id}/plan/confirm  { edits?: string }
 *   POST /sessions/{id}/plan/reject   { reason?: string }
 *
 * This example assumes the agent will enter `plan_pending_approval` after
 * the first message — adjust the prompt if your config differs.
 */

import { Agent, RecursiveClient } from "../dist/index.mjs";

const baseUrl = process.env.RECURSIVE_BASE_URL ?? "http://127.0.0.1:3000";

const agent = await Agent.create({ baseUrl });
const client = new RecursiveClient({ baseUrl });

try {
  console.log("session :", agent.sessionId);

  const run = await agent.send("Plan: refactor http.rs into smaller modules.");
  // We don't drain the stream here — the server should park in
  // plan_pending_approval as soon as the agent calls exit_plan_mode.
  await run.wait().catch(() => {
    // wait() may reject if the run is paused waiting for plan approval;
    // in that case fall through and inspect session state.
  });

  const detail = await client.getSession(agent.sessionId);
  if (detail.status !== "plan_pending_approval" || !detail.pendingPlan) {
    console.log("session not in plan_pending_approval (status:", detail.status, ")");
    process.exit(0);
  }

  console.log("\n--- pending plan ---");
  console.log(detail.pendingPlan);
  console.log("--------------------\n");

  // Approve as-is. To edit before approving, pass `{ edits: "new plan…" }`.
  const resp = await client.approvePlan(agent.sessionId);
  console.log("approval :", resp.status);

  // Drive the now-approved run to completion.
  const run2 = await agent.send("(continue)");
  await run2.wait();
} finally {
  await agent.close();
}
