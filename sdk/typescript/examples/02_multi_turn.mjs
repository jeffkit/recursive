/**
 * 02-multi-turn — create a session, send two messages, stream output.
 *
 * `agent.send()` dispatches the POST in the background so `Run.stream()`
 * can subscribe to SSE before the server starts emitting events. The
 * SDK awaits the POST inside `run.wait()` so HTTP errors still surface.
 */

import { Agent } from "../dist/index.mjs";

const baseUrl = process.env.RECURSIVE_BASE_URL ?? "http://127.0.0.1:3000";

const agent = await Agent.create({ baseUrl });
console.log("session :", agent.sessionId);

try {
  for (const msg of ["Say hi.", "What did I just ask you?"]) {
    console.log(`\n> ${msg}`);
    const run = await agent.send(msg);
    for await (const ev of run.stream()) {
      if (ev.type === "assistant") {
        for (const block of ev.content) {
          if (block.type === "text") process.stdout.write(block.text);
        }
      }
    }
    const result = await run.wait();
    console.log(`\n[finish: ${result.finishReason}]`);
  }
} finally {
  await agent.close();
}
