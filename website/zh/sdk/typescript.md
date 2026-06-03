# TypeScript SDK

TypeScript SDK 为 Recursive HTTP API 提供类型化客户端。

## 安装

```bash
npm install recursive-client
# 或
pnpm add recursive-client
```

## 快速开始

```typescript
import { RecursiveClient } from 'recursive-client';

const client = new RecursiveClient({ baseUrl: 'http://localhost:3000' });

// 健康检查
const status = await client.health();
console.log(status); // "ok"

// 无状态运行
const result = await client.run({ message: '列出 src/ 的文件' });
console.log(result.finishReason);
console.log(result.finalMessage);
```

## 会话管理

```typescript
// 创建会话
const session = await client.createSession({
  systemPrompt: '你是一个有用的 Rust 助手。',
  workspace: '/path/to/project',
});

// 发送消息
const result = await session.run('agent.rs 是做什么的？');
console.log(result.finalMessage);

// 清理
await session.delete();
```

## 流式输出

```typescript
for await (const event of session.runStream('列出所有工具')) {
  if (event.type === 'tool_start') {
    console.log(`[工具] ${event.data.name}`);
  } else if (event.type === 'done') {
    console.log(event.data.finalMessage);
    break;
  }
}
```

## AgentResult

```typescript
interface AgentResult {
  finishReason: 'done' | 'budget_exceeded' | 'stuck' | 'error';
  finalMessage: string | null;
  steps: number;
  tokenUsage?: { prompt: number; completion: number; total: number };
}
```
