/**
 * 01-prompt — one-shot prompt against a running Recursive server.
 *
 * Prerequisites:
 *   1. Start the server in another shell:
 *        cargo run --bin recursive -- http
 *   2. Build the SDK once:
 *        npm run build
 *   3. Run this example:
 *        node examples/01_prompt.mjs
 *      (or, if you tweak it, recompile via `npm run build` first)
 */

import { Agent } from "../dist/index.mjs";

const baseUrl = process.env.RECURSIVE_BASE_URL ?? "http://127.0.0.1:3000";

console.log(`→ POST ${baseUrl}/run`);
const result = await Agent.prompt("List the files in the current directory.", {
  baseUrl,
  maxSteps: 5,
});

console.log("status       :", result.status);
console.log("finishReason :", result.finishReason);
console.log("session id   :", result.id);
if (result.usage) {
  console.log(
    "usage        :",
    `${result.usage.inputTokens} in / ${result.usage.outputTokens} out`,
  );
}
if (!result.ok) {
  console.error("error        :", result.error);
  process.exit(1);
}
