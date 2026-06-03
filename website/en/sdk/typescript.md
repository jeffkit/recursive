# TypeScript SDK

The TypeScript SDK provides a typed client for the Recursive HTTP API, compatible with Claude Agent SDK patterns.

::: tip Package name
The package is published as `@recursive/sdk` on npm. Install with `npm install @recursive/sdk`.
If the published version is not yet available, install directly from source:
```bash
pnpm install   # from sdk/typescript/
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

## Quick start

```typescript
import { RecursiveClient } from '@recursive/sdk';

const client = new RecursiveClient({ baseUrl: 'http://localhost:3000' });

// Health check
const status = await client.health();
console.log(status); // "ok"

// Stateless run
const result = await client.run({ message: 'list files in src/' });
console.log(result.finish_reason);
console.log(result.final_message);
```

## Session management

```typescript
// Create a session
const session = await client.createSession({
  systemPrompt: 'You are a helpful Rust assistant.',
  workspace: '/path/to/project',
});

// Send a message
const result = await session.run('what does agent.rs do?');
console.log(result.finalMessage);

// Continue the conversation
const result2 = await session.run('what are the main entry points?');

// Clean up
await session.delete();
```

## Streaming

```typescript
for await (const event of session.runStream('list all tools')) {
  if (event.type === 'tool_start') {
    console.log(`[tool] ${event.data.name}`);
  } else if (event.type === 'done') {
    console.log(event.data.finalMessage);
    break;
  }
}
```

## API Reference

### `RecursiveClient`

```typescript
const client = new RecursiveClient({
  baseUrl: 'http://localhost:3000',
  apiKey?: string,
  timeout?: number,   // ms, default 60000
});
```

| Method | Description |
|---|---|
| `client.health()` | Returns `"ok"` if healthy |
| `client.tools()` | Returns tool definitions |
| `client.run(options)` | Stateless run |
| `client.createSession(options)` | Create a session |
| `client.listSessions()` | List sessions |
| `client.getSession(id)` | Get session by ID |

### `AgentResult`

```typescript
interface AgentResult {
  finish_reason: 'done' | 'budget_exceeded' | 'stuck' | 'error';
  final_message: string | null;
  steps: number;
  token_usage?: { prompt: number; completion: number; total: number };
}
```
