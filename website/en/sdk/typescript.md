# TypeScript SDK

The TypeScript SDK provides a typed client for the Recursive HTTP API.

## Installation

```bash
npm install recursive-client
# or
pnpm add recursive-client
```

## Quick start

```typescript
import { RecursiveClient } from 'recursive-client';

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
  apiKey?: string,      // optional X-API-Key header
  timeout?: number,     // request timeout in ms (default 60000)
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

### `Session`

| Method | Description |
|---|---|
| `session.run(message)` | Send message, return `AgentResult` |
| `session.runStream(message)` | Returns `AsyncIterator<StepEvent>` |
| `session.delete()` | Delete this session |

### `AgentResult`

```typescript
interface AgentResult {
  finishReason: 'done' | 'budget_exceeded' | 'stuck' | 'error';
  finalMessage: string | null;
  steps: number;
  tokenUsage?: { prompt: number; completion: number; total: number };
}
```
