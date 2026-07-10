# TypeScript SDK

The TypeScript SDK (`@recursive/sdk`) provides a typed client for the Recursive
agent. By default it spawns the local `recursive` CLI with Claude-compatible
`--output-format stream-json`. Pass `baseUrl` (or set `RECURSIVE_BASE_URL`) to
use the HTTP API against a running server instead.

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

Install the `recursive` CLI and ensure it is on `PATH` (or set `RECURSIVE_BIN`).
No HTTP server is required for the default transport.

## Quick start — one-shot

```typescript
import { Agent } from '@recursive/sdk';

const result = await Agent.prompt(
  'List the files in the current directory.',
  { maxSteps: 5 },
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

await using agent = await Agent.create();
console.log('session:', agent.sessionId);

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

const run2 = await agent.send('What are the main entry points?');
const result2 = await run2.wait();
console.log(result2.result);
```

## HTTP transport (optional)

```typescript
await using agent = await Agent.create({ baseUrl: 'http://localhost:3000' });
```

Start the server first:

```bash
recursive http --addr 127.0.0.1:3000
```

## API Reference

### `Agent` (static methods)

| Method | Description |
|---|---|
| `Agent.prompt(message, options?)` | One-shot: CLI by default. Returns `RunResult`. |
| `Agent.create(options?)` | Create a persistent `AgentSession`. |
| `Agent.resume(sessionId, options?)` | Attach to an existing session. |
| `Agent.listSessions(pagination?, options?)` | List sessions (**HTTP only**). |
| `Agent.deleteSession(sessionId, options?)` | Delete a session (**HTTP only**). |

### `RunResult`

```typescript
interface RunResult {
  id: string;
  status: 'finished' | 'error' | 'cancelled';
  subtype: 'success' | 'error_max_turns' | 'error_during_execution' | 'cancelled';
  finishReason?: string;
  usage?: UsageMeta;
  error?: string;
  result?: string;
  numTurns?: number;
  durationMs?: number;
  ok: boolean;
}
```
