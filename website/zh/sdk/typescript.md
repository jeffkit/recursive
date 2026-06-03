# TypeScript SDK

TypeScript SDK 为 Recursive HTTP API 提供类型化客户端，兼容 Claude Agent SDK 接口风格。

::: tip 包名
包发布名为 `@recursive/sdk`，通过 `npm install @recursive/sdk` 安装。
如尚未发布，可从源码本地安装：
```bash
pnpm install   # 在 sdk/typescript/ 目录下执行
```
:::

## 安装

```bash
npm install @recursive/sdk
# 或
pnpm add @recursive/sdk
```

## 前置条件

先启动 Recursive HTTP 服务器：

```bash
recursive http --addr 127.0.0.1:3000
```

## 快速开始

```typescript
import { RecursiveClient } from '@recursive/sdk';

const client = new RecursiveClient({ baseUrl: 'http://localhost:3000' });

// 健康检查
const status = await client.health();
console.log(status); // "ok"

// 无状态运行
const result = await client.run({ message: '列出 src/ 的文件' });
console.log(result.finish_reason);
console.log(result.final_message);
```

## 会话管理

```typescript
const session = await client.createSession({
  systemPrompt: '你是一个有用的 Rust 助手。',
  workspace: '/path/to/project',
});

const result = await session.run('agent.rs 是做什么的？');
console.log(result.finalMessage);

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
