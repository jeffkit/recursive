# @recursive/sdk

TypeScript SDK for the [Recursive Agent](https://github.com/recursive-agent/recursive).

Default transport spawns the local `recursive` CLI with Claude-compatible
`--output-format stream-json`.

## Install

```bash
npm install @recursive/sdk
```

## Claude-compatible `query()` (recommended)

Same shape as `@anthropic-ai/claude-agent-sdk`:

```typescript
import { query } from "@recursive/sdk";

for await (const message of query({
  prompt: "Find and fix the bug in auth.ts",
  options: {
    maxTurns: 10,
    permissionMode: "bypassPermissions",
    cwd: process.cwd(),
    canUseTool: async (tool, input) => ({ behavior: "allow" }),
    allowedTools: ["Read", "Write", "Bash"],
  },
})) {
  if (message.type === "assistant") {
    // message.message.content — Anthropic-style blocks
  }
  if (message.type === "result") {
    // Terminal result is IN the stream (Claude contract)
    console.log(message.subtype, message.result);
  }
}

// Cancel mid-run / stream follow-ups:
const q = query({ prompt: "..." });
await q.interrupt();
await q.streamInput((async function* () { yield "continue"; })());
```

`query()` opens the CLI **control channel** (no `-H`): `--output-format stream-json`
plus `--input-format stream-json`, so `canUseTool`, hooks, `interrupt`, and
multi-turn `streamInput` work like the Claude Agent SDK.

### Options (Claude names)

| Option | Maps to |
|--------|---------|
| `cwd` | `--workspace` |
| `model` | `-m` |
| `maxTurns` | `--max-steps` |
| `systemPrompt` | `--system-prompt` / append |
| `permissionMode` | `--permission-mode` (`bypassPermissions` → `auto`) |
| `resume` | `-r` |
| `pathToClaudeCodeExecutable` | binary path |
| `abortController` | cancel child |
| `maxBudgetUsd` | `--max-budget-usd` |
| `allowedTools` | `--allow-tools` |
| `canUseTool` | control `can_use_tool` replies |
| `hooks` | `initialize` + `hook_callback` |

## Session-style API (also available)

```typescript
import { Agent } from "@recursive/sdk";

const result = await Agent.prompt("List TODOs");
await using agent = await Agent.create();
await (await agent.send("Fix tests")).wait();
```

## HTTP transport (optional)

```typescript
await using agent = await Agent.create({ baseUrl: "http://localhost:3000" });
```

## Environment variables

| Variable | Description |
|----------|-------------|
| `RECURSIVE_BIN` | Path to the `recursive` binary |
| `RECURSIVE_BASE_URL` | When set, `Agent.*` uses HTTP |
| `RECURSIVE_API_KEY` | HTTP auth key |
