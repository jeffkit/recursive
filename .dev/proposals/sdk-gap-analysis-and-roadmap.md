# Proposal: Recursive SDK — Claude Agent SDK 对比与 Gap 分析

> **Status**: Draft — pending review
> **Created**: 2026-06-01
> **Baseline**: Recursive v0.6.0 / `@anthropic-ai/claude-agent-sdk` v0.1.77
> **Scope**: Phase 19 (Ecosystem & Distribution) — SDK 对齐方向

---

## 参考对象澄清

本文档对比的是 **`@anthropic-ai/claude-agent-sdk`**（v0.1.77，已安装于本机）。

- 安装路径：`…/agentstudio/node_modules/@anthropic-ai/claude-agent-sdk`
- **不是** `@anthropic-ai/sdk`（Anthropic API 客户端）
- **不是** `@cursor/sdk`（Cursor 的 Agent SDK）

### 架构差异（根本性）

| 维度 | Claude Agent SDK | Recursive SDK |
|------|-----------------|---------------|
| **运行方式** | 以子进程方式启动 `claude` CLI | 通过 HTTP 访问已运行的 recursive server |
| **通信协议** | 进程 stdio + 流式 JSON | HTTP REST API + SSE |
| **会话载体** | 子进程生命周期 / JSONL 文件 | Server 内存 + 持久化存储 |
| **工具权限** | `canUseTool` 回调（进程内） | 服务端配置（HTTP 层） |
| **MCP 注入** | 可在每次调用时动态传入 `mcpServers` | 服务端全局配置 |

这一架构差异意味着有些特性**天然不等价**，不能 1:1 移植。

---

## 公开 API 对比

### 1. 顶层函数

| 函数 | Claude Agent SDK | Recursive SDK | 状态 |
|------|-----------------|---------------|------|
| 一次性运行（V1） | `query({ prompt, options })` → `Query` | — | ❌ Recursive 无对应 V1 模式 |
| 一次性运行（V2） | `unstable_v2_prompt(msg, opts)` → `Promise<SDKResultMessage>` | `Agent.prompt(msg, opts)` → `RunResult` | ✅ 对齐（返回值结构有差异，见下） |
| 创建 session（V2） | `unstable_v2_createSession(opts)` → `SDKSession` | `Agent.create(opts)` → `AgentSession` | ✅ 对齐 |
| 继续 session（V2） | `unstable_v2_resumeSession(id, opts)` → `SDKSession` | `Agent.resume(id, opts)` → `AgentSession` | ✅ 对齐 |
| 列出 sessions | `listSessions(opts?)` *(fake-cc 源码中，v0.1.77 尚未发布)* | `Agent.listSessions(opts?)` | ✅ 已在 Recursive SDK 中实现（提前覆盖） |
| 获取 session 详情 | `getSessionInfo(id, opts?)` *(尚未发布)* | ❌ | 🔴 Gap |
| 读取历史消息 | `getSessionMessages(id, opts?)` *(尚未发布)* | ❌ | 🔴 Gap |
| 重命名 session | `renameSession(id, title, opts?)` *(尚未发布)* | ❌ | 🔴 Gap |
| 打标签 | `tagSession(id, tag, opts?)` *(尚未发布)* | ❌ | 🟡 低优先 |
| 分叉 session | `forkSession(id, opts?)` *(尚未发布)* | ❌ | 🔴 Gap |
| 删除 session | ❌（不暴露） | `Agent.deleteSession(id)` | 🔵 Recursive 独有 |
| 自定义工具 | `tool(name, desc, schema, handler)` | ❌（工具在服务端注册） | 🔴 架构差异 |
| 创建 MCP 服务 | `createSdkMcpServer(opts)` | ❌ | 🔴 架构差异 |

---

### 2. `query()` — V1 API（当前 SDK 主力接口）

Claude Agent SDK 当前发布版本（v0.1.77）的主 API 是 `query()`，V2 的 `unstable_v2_*` 仍是实验性。

`query()` 返回一个 `Query` 对象，它同时是 `AsyncGenerator<SDKMessage>` 并额外带有控制方法：

| `Query` 方法 | 含义 | Recursive SDK | 状态 |
|-------------|------|---------------|------|
| `for await (msg of query)` | 流式迭代消息 | `run.stream()` / `run.messages()` | ✅ 对齐 |
| `interrupt()` | 中断当前 turn | ❌ | 🔴 Gap |
| `setPermissionMode(mode)` | 运行时切换权限模式 | ❌ | 🔴 Gap（服务端全局配置） |
| `setModel(model?)` | 运行时切换模型 | ❌ | 🔴 Gap（需服务端支持） |
| `setMaxThinkingTokens(n)` | 控制思考 token 上限 | ❌ | 🔴 Gap |
| `supportedCommands()` | 查询可用 slash commands | ❌ | 🟡 低优先 |
| `supportedModels()` | 查询可用模型列表 | ❌ | 🟡 低优先 |
| `mcpServerStatus()` | 查询 MCP 连接状态 | ❌ | 🟡 低优先 |
| `accountInfo()` | 查询账号信息 | ❌ | 🟡 低优先 |
| `rewindFiles(msgId)` | 文件回溯到某条消息时的状态 | ❌ | 🟡 低优先 |
| `setMcpServers(servers)` | 动态替换 MCP 服务器集合 | ❌ | 🔴 架构差异 |
| `streamInput(stream)` | 向 query 流式注入用户消息 | ❌ | 🔴 架构差异 |

> **关键差异**：Recursive 的 `Run` 类对应 `Query`，但目前只覆盖了流式迭代，缺少所有控制方法。

---

### 3. `SDKSession` — V2 会话接口

| 属性/方法 | Claude Agent SDK | Recursive SDK | 状态 |
|-----------|-----------------|---------------|------|
| `sessionId` | ✅ (readonly, 首条消息后可用) | ✅ | ✅ |
| `send(msg)` | ✅ `Promise<void>` | ✅ Python: `send()` → `Run`; TS: `send()` → `Run` | ⚠️ 返回值不同（Recursive 返回 Run） |
| `stream()` | ✅ `AsyncGenerator<SDKMessage>` | ✅ `Run.stream()` | ✅ |
| `close()` | ✅ | ✅ | ✅ |
| `[Symbol.asyncDispose]()` | ✅ | ✅ (TS) / `__exit__` (Py) | ✅ |

> **差异**：Claude SDK 的 `send()` 是 `Promise<void>`，消息通过 `stream()` 获取。Recursive 的 `send()` 直接返回 `Run`，更接近 Cursor SDK 的风格，实际使用更方便。

---

### 4. `SDKMessage` 消息类型对比

Claude Agent SDK v0.1.77 的 `SDKMessage` 是一个 **11 种类型的 union**：

```typescript
type SDKMessage =
  | SDKAssistantMessage      // type: 'assistant'
  | SDKUserMessage           // type: 'user'
  | SDKUserMessageReplay     // type: 'user', isReplay: true
  | SDKResultMessage         // type: 'result', subtype: 'success' | 'error_*'
  | SDKSystemMessage         // type: 'system', subtype: 'init' | 'compact_boundary' | 'status' | 'hook_response'
  | SDKPartialAssistantMessage // type: 'stream_event'
  | SDKCompactBoundaryMessage  // type: 'system', subtype: 'compact_boundary'
  | SDKStatusMessage           // type: 'system', subtype: 'status'
  | SDKHookResponseMessage     // type: 'system', subtype: 'hook_response'
  | SDKToolProgressMessage     // type: 'tool_progress'
  | SDKAuthStatusMessage       // type: 'auth_status'
```

Recursive SDK 目前只结构化处理 3 种，其他透传为 `raw_data`：

| 消息类型 | Claude SDK 字段 | Recursive SDK | 状态 |
|---------|----------------|---------------|------|
| `assistant` | `message`(完整 Anthropic message), `parent_tool_use_id`, `error?`, `uuid`, `session_id` | 简化版（`content: ContentBlock[]`，无 `parent_tool_use_id`） | ⚠️ 字段不完整 |
| `user` | `message`(APIUserMessage), `parent_tool_use_id`, `isSynthetic?`, `tool_use_result?` | 简化版（只有 `text`） | ⚠️ 字段不完整 |
| `result`（成功） | `subtype:"success"`, `result`(最终文字), `num_turns`, `duration_ms`, `total_cost_usd`, `usage`, `modelUsage`, `permission_denials` | 未作为 stream 消息发出，信息分散在 `RunResult` 中 | 🔴 结构不对齐 |
| `result`（错误） | `subtype:"error_during_execution/error_max_turns/error_max_budget_usd/..."`, `errors[]` | `RunResult.status == "error"` | ⚠️ 错误子类型缺失 |
| `system/init` | `tools[]`, `mcp_servers[]`, `model`, `permissionMode`, `cwd`, `claude_code_version` | 透传 raw_data | ⚠️ 无结构化 |
| `stream_event` | 原始 Anthropic streaming delta（RawMessageStreamEvent） | ❌ 未透出 | 🔴 Gap |
| `system/compact_boundary` | `compact_metadata.trigger`, `pre_tokens` | ❌ 未透出 | 🟡 中优先 |
| `system/status` | `status: 'compacting' \| null` | ❌ | 🟡 中优先 |
| `system/hook_response` | `hook_name`, `stdout`, `stderr`, `exit_code` | ❌ | 🟡 中优先 |
| `tool_progress` | `tool_use_id`, `tool_name`, `elapsed_time_seconds` | ❌ | 🔴 Gap |
| `auth_status` | `isAuthenticating`, `output[]`, `error?` | ❌ N/A | 🟡 低优先 |

---

### 5. `SDKResultMessage` 与 `RunResult` 字段对比

| 字段 | Claude Agent SDK `SDKResultMessage` | Recursive `RunResult` | 状态 |
|------|-------------------------------------|-----------------------|------|
| `type: 'result'` | ✅ | ❌（不在 stream 里） | 🔴 结构不对齐 |
| `subtype` | `'success' \| 'error_during_execution' \| 'error_max_turns' \| 'error_max_budget_usd'` | `status: 'finished'\|'error'\|'cancelled'` | ⚠️ 错误子类型缺失 |
| `result` | ✅ **最终文字结果**（完整助手回答） | ❌ 需自己从 stream 拼接 | 🔴 Gap |
| `num_turns` | ✅ | ❌ | 🟡 |
| `duration_ms` | ✅ | ❌ | 🟡 |
| `duration_api_ms` | ✅ | ❌ | 🟡 |
| `is_error` | ✅ | ❌ | 🟡 |
| `total_cost_usd` | ✅ | ❌（Recursive 无计费模块） | 🟢 低 |
| `usage` | ✅ 完整 `NonNullableUsage` | ✅ `UsageMeta` | ✅ |
| `modelUsage` | ✅ 按模型分组 | ❌ | 🟡 |
| `permission_denials` | ✅ 被拒绝工具列表 | ❌ | 🟡 |
| `structured_output` | ✅ (配合 outputFormat 使用) | ❌ | 🟢 低 |
| `uuid` / `session_id` | ✅ | ✅ `id` | ✅ |

---

### 6. `Options` — 调用选项对比

Claude Agent SDK 的 `Options` 有 **30+ 个参数**，以下是最重要的分组：

**Recursive 已支持：**
- `model` → `AgentOptions.model` ✅
- `resume` → `Agent.resume(id)` ✅
- `systemPrompt` → `AgentOptions.system_prompt` ✅
- `maxTurns` → `AgentOptions.max_turns` ✅

**Recursive 尚未支持的高价值选项：**

| 选项 | 含义 | 优先级 |
|------|------|--------|
| `abortController` | 取消控制器，实现 `cancel()` | 🔴 高 |
| `mcpServers` | 每次调用动态注入 MCP 服务器 | 🔴 架构差异（服务端全局 vs 调用级） |
| `canUseTool` | 工具权限回调函数 | 🔴 架构差异（服务端 vs 进程内） |
| `permissionMode` | 权限模式 (`default/acceptEdits/bypassPermissions`) | 🟡 服务端需支持 |
| `hooks` | PreToolUse/PostToolUse 等钩子回调 | 🟡 架构差异 |
| `includePartialMessages` | 是否输出流式 delta 事件 | 🟡 中优先 |
| `maxThinkingTokens` | 思考 token 上限 | 🟡 需 LLM 层支持 |
| `maxBudgetUsd` | 预算上限（用量控制） | 🟢 低 |
| `outputFormat` | 结构化输出（JSON Schema） | 🟢 低 |
| `sandbox` | 沙箱隔离配置 | 🟢 低 |
| `enableFileCheckpointing` | 文件回溯功能 | 🟢 低 |
| `persistSession` | 是否持久化 session | 🟢 低 |
| `forkSession` | resume 时分叉为新 session | 🟡 中优先 |
| `betas` | Beta 功能开关 | 🟢 低 |
| `agents` | 自定义 sub-agents 定义 | 🟢 低 |
| `plugins` | 插件加载 | 🟢 低 |

---

### 7. 架构差异导致的天然 Gap

以下 Gap 不是实现问题，而是由两者**运行模型差异**决定的：

| Claude Agent SDK | Recursive SDK 现状 | 分析 |
|-----------------|-------------------|------|
| 启动 `claude` 子进程，stdio 通信 | HTTP 访问已运行的服务器 | 天然不同，无法 1:1 映射 |
| `canUseTool` 进程内权限回调 | 服务端 TOML 配置 / TUI 弹窗 | 需要 HTTP 权限协商协议 |
| `mcpServers` 每次调用注入 | 服务端 global MCP 配置 | 需要 session 级 MCP 管理 |
| `spawnClaudeCodeProcess` 自定义 spawn | 固定 HTTP 地址 | 可通过 `base_url` 配置对应 |
| `hooks` 进程内钩子函数 | 服务端 hook 事件通知 | 可以做 SSE hook 事件透出 |

---

## 分阶段推进建议

### Phase A — 纯 SDK 层补齐（1-2 周，不需后端改动）

1. **`RunResult.result` 字段**：`run.wait()` 内部收集所有 `assistant` 消息文字拼接后填入，Python + TypeScript 同步。
2. **`Run.interrupt()` 接口（前端先占位）**：SDK 侧增加方法，内部调用 `DELETE /sessions/{id}` 模拟（有损），标记为 TODO 等后端支持专用端点。
3. **`RunResult.num_turns` / `duration_ms`**：从 stream 事件中统计，Python 用 `time.time()` 测量。
4. **错误子类型细化**：`RunResult.subtype` 增加 `error_max_turns` / `error_max_budget_usd` 等枚举，需要后端在 SSE 结束事件中携带。
5. **`getSessionMessages(sessionId)`**：包装已有的 `GET /sessions/{id}` 返回的 messages 字段。

### Phase B — 中断与实时进度（2-3 周，需后端配合）

1. **后端增加 `POST /sessions/{id}/interrupt`** 端点（给当前 turn 发送取消信号）。
2. **SDK 增加真正的 `run.cancel()`**：调用 interrupt 端点，结束 SSE 迭代，状态设为 `cancelled`。
3. **`tool_progress` 消息透出**：后端 SSE `/events` 增加 `tool_progress` 类型事件，SDK 映射为 `SDKToolProgressMessage`。
4. **`stream_event` delta 透出**（可选）：`includePartialMessages: true` 时发出原始 streaming delta，用于 UI 打字机效果。

### Phase C — Session 管理完善（3-4 周，需后端配合）

1. **`forkSession(sessionId, opts?)`**：后端新增 `POST /sessions/{id}/fork`，SDK 封装。
2. **`renameSession(sessionId, title)`**：后端 `PATCH /sessions/{id}` 支持更新 title，SDK 封装。
3. **`getSessionInfo(sessionId)`**：后端 `GET /sessions/{id}` 补充 `lastModified / cwd / gitBranch` 字段，SDK 封装。
4. **`listSessions` 分页**：增加 `limit / offset` 参数。

### Phase D — 架构级特性（评估后决定）

1. **Session 级 MCP 注入**：允许在 `Agent.create()` 时传入 `mcpServers`，需要重设计服务端 MCP 管理模块。
2. **HTTP 权限协商协议**：允许 SDK 调用方提供 `canUseTool` 回调（通过 SSE/回调 HTTP 端点），服务端工具执行前请示 SDK。
3. **进程内钩子透出**：将服务端 hook 事件通过 SSE 发出，SDK 调用方可注册 `hooks` 回调。

---

## 优先级汇总

| 优先级 | Gap 描述 | Phase | 需后端改动 | 架构差异 |
|--------|---------|-------|-----------|--------|
| 🔴 高 | `RunResult.result` 最终文字 | A | 否 | 否 |
| 🔴 高 | `run.cancel()` / `run.interrupt()` | A→B | 是（需专用端点） | 否 |
| 🔴 高 | `getSessionMessages(sessionId)` | A | 否（已有数据） | 否 |
| 🔴 高 | `tool_progress` 消息（工具执行进度） | B | 是（SSE 扩展） | 否 |
| 🟡 中 | `RunResult.num_turns` / `duration_ms` | A | 否 | 否 |
| 🟡 中 | `SDKResultMessage` 错误子类型细化 | B | 是 | 否 |
| 🟡 中 | `forkSession()` | C | 是（新端点） | 否 |
| 🟡 中 | `renameSession()` | C | 是 | 否 |
| 🟡 中 | `getSessionInfo()` 字段完善 | C | 是 | 否 |
| 🟡 中 | `includePartialMessages` / `stream_event` | B | 是 | 否 |
| 🔴 架构 | `canUseTool` 权限回调 | D | 是（协议设计） | **是** |
| 🔴 架构 | `mcpServers` 调用级注入 | D | 是（模块重设计） | **是** |
| 🟢 低 | `setModel()` 运行时切换模型 | D | 是 | 否 |
| 🟢 低 | `outputFormat` 结构化输出 | D | 是 | 否 |
| 🟢 低 | `total_cost_usd` | D | 是（需计费） | 否 |

---

## 待决策问题

1. **Phase A 立刻开工？** 以下几项不需要后端改动，可本周完成：`RunResult.result`、`getSessionMessages`、`num_turns`/`duration_ms`。
2. **`run.cancel()` 的降级方案**：Phase B 后端就绪前，是否接受"cancel = delete session"的有损方案？
3. **SDKSession.send() 返回值**：Claude SDK 是 `Promise<void>`；Recursive 返回 `Run`。后者更方便，但不对齐。是否保持 Recursive 的设计？
4. **Phase D 是否进入 v0.6.x 范畴**？架构级改动（MCP inline / canUseTool 协议）工作量大，是否推迟到 v0.7.0？
5. **SDK 发布策略**：当前 Python SDK 和 TypeScript SDK 是否已足够稳定发布 beta 到 PyPI / npm？

---

*提案生成时间：2026-06-01*
*参考版本：`@anthropic-ai/claude-agent-sdk` v0.1.77，类型文件路径 `entrypoints/sdk/{coreTypes,runtimeTypes}.d.ts`*
*关联会话：[SDK 对齐实现](404dd214-d736-437a-aa48-37504e878868)*
