# @recursive/sdk

TypeScript SDK for the [Recursive Agent](https://github.com/recursive-agent/recursive).

API-compatible with the Claude Agent SDK / Cursor SDK patterns.

## Install

```bash
npm install @recursive/sdk
# or
pnpm add @recursive/sdk
# or
yarn add @recursive/sdk
```

## Prerequisites

Start the Recursive server:

```bash
recursive loop --http 3000
```

Set environment (if auth is enabled):

```bash
export RECURSIVE_API_KEY=your-key
export RECURSIVE_BASE_URL=http://localhost:3000  # optional
```

## Usage

### One-shot (`Agent.prompt`)

```typescript
import { Agent } from "@recursive/sdk";

const result = await Agent.prompt(
  "List all TODO comments across the codebase",
  { baseUrl: "http://localhost:3000" },
);

console.log(result.status);       // "finished" | "error" | "cancelled"
console.log(result.finishReason);
if (result.ok) {
  console.log("Success!");
}
```

### Multi-turn with streaming (`Agent.create` + `agent.send`)

```typescript
import { Agent } from "@recursive/sdk";

// `await using` auto-disposes on block exit (TypeScript 5.2+)
await using agent = await Agent.create({
  baseUrl: "http://localhost:3000",
});

// First turn — stream tokens as they arrive
const run = await agent.send("Fix all failing tests in the project");
for await (const msg of run.stream()) {
  if (msg.type === "assistant") {
    for (const block of msg.content) {
      if (block.type === "text") process.stdout.write(block.text);
    }
  }
}
const result = await run.wait();
console.log(`\n[${result.status}]`);

// Follow-up — same conversation context
const run2 = await agent.send("Now update CHANGELOG.md");
await run2.wait();
```

### Resume an existing session (`Agent.resume`)

```typescript
import { Agent } from "@recursive/sdk";

await using agent = await Agent.resume(sessionId, {
  baseUrl: "http://localhost:3000",
});
const run = await agent.send("Continue where we left off");
await run.wait();
```

### Error handling

```typescript
import { Agent, RecursiveAgentError } from "@recursive/sdk";

try {
  await using agent = await Agent.create({ baseUrl: "http://localhost:3000" });
  const run = await agent.send("do something");
  const result = await run.wait();

  if (result.status === "error") {
    // Agent ran but hit an error
    console.error("Run failed:", result.error);
    process.exit(2);
  }
} catch (err) {
  if (err instanceof RecursiveAgentError) {
    // Couldn't connect / auth failed
    console.error("Startup failed:", err.message, "retryable:", err.isRetryable);
    process.exit(1);
  }
  throw err;
}
```

## API Reference

### `Agent` (static factory)

| Method | Description |
|--------|-------------|
| `Agent.prompt(message, options?)` | One-shot run |
| `Agent.create(options?)` | Create a new session |
| `Agent.resume(sessionId, options?)` | Resume existing session |
| `Agent.listSessions(options?)` | List active sessions |
| `Agent.deleteSession(sessionId, options?)` | Delete a session |

### `AgentSession`

| Method | Description |
|--------|-------------|
| `agent.send(message)` | Send a message, returns `Promise<Run>` |
| `agent.close()` | Close the session |
| `agent[Symbol.asyncDispose]()` | Used by `await using` |

### `Run`

| Method | Description |
|--------|-------------|
| `run.stream()` | `AsyncGenerator<SDKMessage>` |
| `run.messages()` | Alias for `stream()` |
| `run.iterText()` | `AsyncGenerator<string>` — text chunks only |
| `run.text()` | `Promise<string>` — all text concatenated |
| `run.wait()` | `Promise<RunResult>` |
| `run.supports(op)` | Check if operation is available |

### `AgentOptions`

Passed to `Agent.create()`, `Agent.resume()`, and `Agent.prompt()`.

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

`PromptOptions` is an alias for `AgentOptions` — all the same options apply to `Agent.prompt()`.

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

Example — append to the default system prompt:

```typescript
const result = await Agent.prompt("Fix all failing tests", {
  baseUrl: "http://localhost:3000",
  appendSystemPrompt: "\nAlways run `cargo test` to verify before finishing.",
});
```

### Message types

```typescript
type SDKMessage = AssistantMessage | UserMessage | SystemMessage;

interface AssistantMessage {
  type: "assistant";
  content: ContentBlock[];  // TextContent | ToolUseBlock | ToolResultBlock
  sessionId: string;
}

interface UserMessage {
  type: "user";
  content: string;
  sessionId: string;
}
```

## Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `RECURSIVE_BASE_URL` | `http://127.0.0.1:3000` | Server URL |
| `RECURSIVE_API_KEY` | _(none)_ | API key for authenticated servers |
