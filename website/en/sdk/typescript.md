# TypeScript SDK

The TypeScript SDK (`@recursive/sdk`) provides a typed client for the Recursive HTTP API, compatible with Claude Agent SDK patterns.

::: tip Package name
The package is published as `@recursive/sdk` on npm. If it is not yet available, install directly from source:
```bash
cd sdk/typescript && pnpm install && pnpm build
```
:::

## Installation

```bash
npm install @recursive/sdk
# or
pnpm add @recursive/sdk
```

## Prerequisites

Start the Recursive HTTP server first:

```bash
recursive http --addr 127.0.0.1:3000
```

## Quick start — one-shot

```typescript
import { Agent } from '@recursive/sdk';

const result = await Agent.prompt(
  'List the files in the current directory.',
  { baseUrl: 'http://localhost:3000', maxSteps: 5 },
);

console.log('status       :', result.status);
console.log('finish_reason:', result.finishReason);
if (result.result) {
  console.log('answer       :', result.result);
}
```

## Multi-turn session

```typescript
import { Agent } from '@recursive/sdk';

// await using (TypeScript 5.2+) auto-closes the session
await using agent = await Agent.create({ baseUrl: 'http://localhost:3000' });
console.log('session:', agent.sessionId);

// First turn
const run = await agent.send('What does agent.rs do?');
for await (const msg of run.stream()) {
  if (msg.type === 'assistant') {
    for (const block of msg.content) {
      if (block.type === 'text') process.stdout.write(block.text);
    }
  }
}
const result = await run.wait();
console.log('\n[finish:', result.finishReason, ']');

// Second turn (same session — context preserved)
const run2 = await agent.send('What are the main entry points?');
const result2 = await run2.wait();
console.log(result2.result);
```

## Streaming events

```typescript
import { Agent } from '@recursive/sdk';

await using agent = await Agent.create({ baseUrl: 'http://localhost:3000' });
const run = await agent.send('Summarise src/');

for await (const msg of run.stream()) {
  if (msg.type === 'assistant') {
    for (const block of msg.content) {
      if (block.type === 'text') process.stdout.write(block.text);
    }
  } else if (msg.type === 'tool_call') {
    console.log(`\n[tool] ${msg.name}`);
  }
}

const result = await run.wait();
console.log(`\nDone in ${result.numTurns} turns`);
```

## Session options (`AgentOptions`)

Passed to `Agent.create()`, `Agent.resume()`, and `Agent.prompt()`:

```typescript
interface AgentOptions {
  baseUrl?: string;            // default: RECURSIVE_BASE_URL or http://127.0.0.1:3000
  apiKey?: string;             // default: RECURSIVE_API_KEY env var
  timeout?: number;            // ms, default 120_000
  systemPrompt?: string;       // replace server's default system prompt
  appendSystemPrompt?: string; // append to default (ignored if systemPrompt is set)
  sessionName?: string;        // human-readable display name
  maxSteps?: number;           // max agent steps
  planningMode?: "immediate" | "plan_first"; // default: "immediate"
  thinkingBudget?: number;     // extended-thinking token budget; 0 = disabled
  permissionMode?: "default" | "auto" | "strict" | "bypass";
  maxBudgetUsd?: number;       // max API spend in USD
}
```

Example — Plan Mode + named session:

```typescript
await using agent = await Agent.create({
  baseUrl: "http://localhost:3000",
  sessionName: "refactor-auth",
  planningMode: "plan_first",
  maxSteps: 20,
});
const run = await agent.send("Refactor the auth module to use JWTs");
await run.wait();
```

Example — append extra instructions:

```typescript
const result = await Agent.prompt("Fix all failing tests", {
  baseUrl: "http://localhost:3000",
  appendSystemPrompt: "\nAlways run `cargo test` to verify before finishing.",
});
```

## API Reference

### `Agent` (static methods)

| Method | Description |
|---|---|
| `Agent.prompt(message, options?)` | One-shot: create session, send, wait, delete. Returns `RunResult`. |
| `Agent.create(options?)` | Create a persistent `AgentSession`. Use `await using` for cleanup. |
| `Agent.resume(sessionId, options?)` | Attach to an existing session. |
| `Agent.listSessions(options?)` | List active sessions. |
| `Agent.deleteSession(sessionId, options?)` | Delete a session. |

### `AgentSession`

| Method | Description |
|---|---|
| `agent.send(message)` | Send a message and return a `Run`. |
| `agent.sessionId` | The session ID. |
| `agent[Symbol.asyncDispose]()` | Auto-called by `await using`. |

### `Run`

| Method | Description |
|---|---|
| `run.wait()` | Resolves when the run completes. Returns `RunResult`. |
| `run.stream()` | `AsyncIterableIterator` of streaming message events. |

### `RunResult`

```typescript
interface RunResult {
  id: string;
  status: 'finished' | 'error' | 'cancelled';
  subtype: 'success' | 'error_max_turns' | 'error_during_execution' | 'cancelled';
  finishReason?: string;
  usage?: UsageMeta;
  error?: string;
  result?: string;          // Concatenated final assistant text
  numTurns?: number;
  durationMs?: number;
  ok: boolean;              // true when status === 'finished'
}
```
