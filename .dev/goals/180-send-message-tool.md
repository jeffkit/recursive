# Goal 180 — `send_message` Tool: Coordinator ↔ Worker Bidirectional Messaging

**Roadmap**: Phase 18 — Advanced Agent Patterns (coordinator pattern)
**Design principle check**:
- 新工具文件 `src/tools/send_message.rs`
- ❌ 不在 `agent.rs::Agent::run` 主循环里加分支
- ✅ 纯新增能力

## Why

当前 `spawn_worker` 是单向的：coordinator 派发任务，等待 worker 完成，无法中途干预。

Fake CC 有 `SendMessageTool`，允许 coordinator **向正在运行的 worker 发送后续指令**：
- Worker 运行中 coordinator 获得了新信息 → 动态调整 worker 的任务
- Worker 完成了第一阶段 → coordinator 发送下一步指令

## Architecture

要实现双向通信，需要：
1. `WorkerRegistry` — 存储正在运行的 worker 的通信通道 (`Arc<RwLock<HashMap<String, Sender>>`)
2. `spawn_worker` 工具：启动 worker 时注册到 registry，返回 `worker_id`
3. `send_message` 工具：向指定 `worker_id` 的 mailbox 发送消息
4. Worker 端：每轮完成后检查 mailbox，将新消息追加到 context 继续运行

## Implementation approach

### Phase A（本 Goal）: Mailbox-based async messaging
- `WorkerMailbox = Arc<Mutex<VecDeque<String>>>` — 每个 worker 一个 mailbox
- `WorkerRegistry = Arc<RwLock<HashMap<String, WorkerMailbox>>>` — 全局注册表
- `spawn_worker` 升级：返回 `worker_id`，注册 mailbox
- `send_message` 工具：向 registry 中的 mailbox 推送消息
- Worker kernel 每步完成后 poll mailbox，注入为 user message 继续

### 工具参数 (`send_message`):
```json
{
  "worker_id": "string — spawn_worker 返回的 ID",
  "message": "string — 要发送的消息内容"
}
```

## 复杂度评估

此 Goal 复杂度较高（L），原因：
- 需要修改 `spawn_worker` 的返回值（现在返回字符串，需要包含 worker_id）
- 需要 Worker 的 kernel 支持 mid-run message injection（目前 AgentKernel 不支持）
- 需要共享的 `WorkerRegistry` 在工具和 kernel 之间传递

**建议分两步实现**：
- 本 Goal 实现 registry + mailbox 基础设施，以及 `send_message` 工具的框架
- 后续 Goal 把 mailbox 接入 AgentKernel（需要修改 kernel turn loop）

## Files to change

| File | Change |
|------|--------|
| `src/tools/send_message.rs` (new) | `WorkerRegistry`, `WorkerMailbox`, `SendMessageTool` |
| `src/tools/mod.rs` | 注册导出 |

## Acceptance

1. `cargo test --workspace` 全绿
2. `SendMessageTool` 能向已注册 mailbox 推送消息，并对未知 worker_id 返回清晰错误
3. `WorkerRegistry` 支持注册、注销、列出 active workers
