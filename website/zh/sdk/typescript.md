# TypeScript SDK

TypeScript SDK（`@recursive/sdk`）为 Recursive HTTP API 提供类型化客户端，兼容 Claude Agent SDK 模式。

::: tip 包名
包在 npm 上发布为 `@recursive/sdk`。如尚未发布，可从源码安装：
```bash
cd sdk/typescript && pnpm install && pnpm build
```
:::

## 安装

```bash
npm install @recursive/sdk
# 或
pnpm add @recursive/sdk
```

## 前置条件

先启动 Recursive HTTP 服务：

```bash
recursive http --addr 127.0.0.1:3000
```

## 快速开始 — 一次性运行

```typescript
import { Agent } from '@recursive/sdk';

const result = await Agent.prompt(
  '列出当前目录中的文件。',
  { baseUrl: 'http://localhost:3000', maxSteps: 5 },
);

console.log('状态        :', result.status);
console.log('finish_reason:', result.finishReason);
if (result.result) {
  console.log('答案        :', result.result);
}
```

## 多轮会话

```typescript
import { Agent } from '@recursive/sdk';

// await using（TypeScript 5.2+）会话结束后自动关闭
await using agent = await Agent.create({ baseUrl: 'http://localhost:3000' });
console.log('session:', agent.sessionId);

// 第一轮
const run = await agent.send('agent.rs 是做什么的？');
for await (const msg of run.stream()) {
  if (msg.type === 'assistant') {
    for (const block of msg.content) {
      if (block.type === 'text') process.stdout.write(block.text);
    }
  }
}
const result = await run.wait();
console.log('\n[完成:', result.finishReason, ']');

// 第二轮（同一会话——上下文保留）
const run2 = await agent.send('主要入口点有哪些？');
const result2 = await run2.wait();
console.log(result2.result);
```

## 流式事件

```typescript
import { Agent } from '@recursive/sdk';

await using agent = await Agent.create({ baseUrl: 'http://localhost:3000' });
const run = await agent.send('总结 src/ 目录');

for await (const msg of run.stream()) {
  if (msg.type === 'assistant') {
    for (const block of msg.content) {
      if (block.type === 'text') process.stdout.write(block.text);
    }
  } else if (msg.type === 'tool_call') {
    console.log(`\n[工具] ${msg.name}`);
  }
}

const result = await run.wait();
console.log(`\n完成，共 ${result.numTurns} 轮`);
```

## 会话选项（`AgentOptions`）

传递给 `Agent.create()`、`Agent.resume()` 和 `Agent.prompt()` 的选项对象：

```typescript
interface AgentOptions {
  baseUrl?: string;            // 默认: RECURSIVE_BASE_URL 或 http://127.0.0.1:3000
  apiKey?: string;             // 默认: RECURSIVE_API_KEY 环境变量
  timeout?: number;            // 毫秒，默认 120_000
  systemPrompt?: string;       // 完全替换服务器的默认系统提示词
  appendSystemPrompt?: string; // 在默认提示词后追加（设置了 systemPrompt 时忽略）
  sessionName?: string;        // 会话可读显示名
  maxSteps?: number;           // 最大步数
  planningMode?: "immediate" | "plan_first"; // 默认: "immediate"
  thinkingBudget?: number;     // 扩展思考 token 预算；0 = 禁用
  permissionMode?: "default" | "auto" | "strict" | "bypass";
  maxBudgetUsd?: number;       // 最大 API 花费（美元）
}
```

示例 — Plan Mode + 命名会话：

```typescript
await using agent = await Agent.create({
  baseUrl: "http://localhost:3000",
  sessionName: "refactor-auth",
  planningMode: "plan_first",
  maxSteps: 20,
});
const run = await agent.send("重构认证模块，改用 JWT");
await run.wait();
```

示例 — 在默认提示词后追加额外指令：

```typescript
const result = await Agent.prompt("修复所有失败的测试", {
  baseUrl: "http://localhost:3000",
  appendSystemPrompt: "\n完成前务必运行 cargo test 验证。",
});
```

## API 参考

### `Agent`（静态方法）

| 方法 | 说明 |
|---|---|
| `Agent.prompt(message, options?)` | 一次性：创建会话、发送、等待、删除。返回 `RunResult`。 |
| `Agent.create(options?)` | 创建持久 `AgentSession`，用 `await using` 自动清理。 |
| `Agent.resume(sessionId, options?)` | 附加到现有会话。 |
| `Agent.listSessions(options?)` | 列出活跃会话。 |
| `Agent.deleteSession(sessionId, options?)` | 删除会话。 |

### `AgentSession`

| 方法 | 说明 |
|---|---|
| `agent.send(message)` | 发送消息，返回 `Run`。 |
| `agent.sessionId` | 会话 ID。 |
| `agent[Symbol.asyncDispose]()` | 由 `await using` 自动调用。 |

### `Run`

| 方法 | 说明 |
|---|---|
| `run.wait()` | 运行完成时 resolve，返回 `RunResult`。 |
| `run.stream()` | 流式消息事件的 `AsyncIterableIterator`。 |

### `RunResult`

```typescript
interface RunResult {
  id: string;
  status: 'finished' | 'error' | 'cancelled';
  subtype: 'success' | 'error_max_turns' | 'error_during_execution' | 'cancelled';
  finishReason?: string;
  usage?: UsageMeta;
  error?: string;
  result?: string;          // 累积的最终助手文本
  numTurns?: number;
  durationMs?: number;
  ok: boolean;              // status === 'finished' 时为 true
}
```
