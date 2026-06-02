/**
 * 05-slash-commands — list registered slash commands (g169).
 *
 * Combines built-in commands and skill-backed commands loaded from
 * `<workspace>/.recursive/skills/` and `~/.recursive/skills/`.
 */

import { RecursiveClient } from "../dist/index.mjs";

const client = new RecursiveClient({
  baseUrl: process.env.RECURSIVE_BASE_URL ?? "http://127.0.0.1:3000",
});

const cmds = await client.listSlashCommands();

console.log(`${cmds.length} slash command(s):\n`);
for (const c of cmds.sort((a, b) => a.name.localeCompare(b.name))) {
  const aliases = c.aliases.length ? ` (aliases: ${c.aliases.join(", ")})` : "";
  const hint = c.argumentHint ? ` ${c.argumentHint}` : "";
  console.log(`  /${c.name}${hint}${aliases}  [${c.source}]`);
  console.log(`      ${c.description}`);
}
