# Proposal: Goal 165 — SDK Subprocess Transport (daemon mode)

> **Status**: Draft — approved for implementation
> **Created**: 2026-06-01
> **Phase**: 19 (Ecosystem & Distribution)
> **Estimated effort**: ~3 weeks (2w Rust + 1w SDK)

---

## 背景与动机

当前 Recursive SDK（Python + TypeScript）建立在 HTTP REST API 之上，用户必须先启动一个 `recursive http` 服务进程才能使用 SDK。这与 `@anthropic-ai/claude-agent-sdk` 的使用体验存在显著差距：

```python
# Claude Agent SDK — 零配置，装完即用
result = await unstable_v2_prompt("帮我写测试", options={"model": "claude-sonnet-4-5"})

# 当前 Recursive SDK — 需要先起服务器
# $ recursive http &
agent = Agent(base_url="http://localhost:8080")
result = await agent.prompt("帮我写测试")
```

用户体验目标：SDK 用户无需手动管理服务器进程。

---

## 架构决策

### 主方向：subprocess transport（v1）

SDK 通过 spawn `recursive` 子进程与 Agent 通信：

```
SDK (Python/TS)
    ↓  spawn
recursive daemon --json
    ├── stdin  ← user messages (NDJSON)
    └── stdout → events (NDJSON StepEvent stream)
```

**关键特性**：子进程**长驻内存**，支持 multi-turn 对话，无需每次重新启动：
- `Agent.create()` → spawn 一次，进程保持存活
- `agent.send(msg)` → 写一行 JSON 到 stdin（微秒级，无启动开销）
- `agent.close()` → shutdown 信号，进程正常退出

### 次选：HTTP transport（v2，已存在）

保留现有 HTTP API 作为"远程 / 多并发客户端"场景：
- CI/CD 中心化 server，多个 SDK 客户端并发访问
- 跨语言共享同一个 recursive server 实例
- 云端部署

SDK 自动探测：`base_url` 未设置时使用 subprocess，设置后使用 HTTP。

---

## 设计规格

### Part 1：`recursive daemon` 子命令（Rust 侧）

#### 子命令定义

```
recursive daemon [OPTIONS]

Options:
  --session-id <ID>   Resume an existing session
  --workspace <DIR>   Workspace root [default: current dir]
  --json              Emit events as NDJSON (always on in daemon mode)
```

#### stdin 协议（每行一个 JSON 对象）

```typescript
// 发送用户消息，开始新 turn
{ "type": "message", "content": "帮我写测试" }

// 中断当前 turn（graceful cancel）
{ "type": "interrupt" }

// 优雅关闭
{ "type": "shutdown" }
```

#### stdout 协议（每行一个 JSON 对象）

复用现有 `AgentEvent` / `StepEvent` JSON 格式，新增以下控制事件：

```typescript
// 启动完成，客户端可以发消息了
{ "type": "ready", "session_id": "uuid-...", "version": "0.6.0" }

// 一个 turn 结束，等待下一条消息
{ "type": "turn_finished", "turns": 1, "steps": 3 }

// 收到 shutdown 或发生不可恢复错误时
{ "type": "closed", "reason": "shutdown" | "error", "message"?: "..." }
```

已有的 `AgentEvent` 类型**不变**，完整透传：

| 事件 | 含义 |
|------|------|
| `assistant_text` | LLM 生成的文字 |
| `tool_call` | 工具调用 |
| `tool_result` | 工具执行结果 |
| `usage` | token 用量 |
| `partial_token` | 流式 delta（streaming 开启时） |
| `turn_finished` | *(新增)* turn 结束 |
| `closed` | *(新增)* 进程退出 |

#### 实现要点

```
src/
  main.rs            — 新增 `daemon` 子命令 CLI 解析
  daemon.rs          — DaemonRunner: stdin reader + event emitter
```

核心循环（伪代码）：
```rust
// 1. 初始化 session（新建或 resume）
// 2. 输出 {"type":"ready","session_id":"..."}
// 3. loop:
//    a. 从 stdin 读取一行 JSON
//    b. match event.type:
//       "message" => run agent turn, emit StepEvents → turn_finished
//       "interrupt" => abort current turn
//       "shutdown" => 输出 closed, 退出
//    c. 遇到 stdin EOF => 优雅关闭
```

**关键不变量**：
- Invariant #1 保持：agent loop 不变，daemon 只是新的驱动层
- `DaemonRunner` 直接调用已有的 `AgentRuntime::run_turn()`
- 不引入新依赖（只用 `std::io`、`tokio` BufReader）

---

### Part 2：SDK 重写（Python + TypeScript）

#### 传输层抽象

引入 `Transport` 接口，支持两种实现：

```python
# Python
class Transport(Protocol):
    async def send(self, message: str) -> None: ...
    async def events(self) -> AsyncIterable[dict]: ...
    async def close(self) -> None: ...

class SubprocessTransport(Transport):
    """spawn recursive daemon --json"""

class HttpTransport(Transport):
    """HTTP REST + SSE（保留现有实现）"""
```

#### 公开 API 保持不变

```python
# 用户代码无需修改
async with Agent.create() as agent:          # 内部 → SubprocessTransport
    run = agent.send("帮我写测试")
    async for msg in run.stream():
        print(msg)
```

```python
# 显式指定 HTTP transport
async with Agent.create(base_url="http://remote:8080") as agent:
    ...
```

#### 自动 binary 探测

`SubprocessTransport` 启动时：
1. 检查 `RECURSIVE_BIN` 环境变量
2. 检查 PATH 中的 `recursive`
3. 如果找不到，抛出 `RecursiveAgentError("recursive binary not found, install with: curl -fsSL ... | sh")`

---

### Part 3：`Query.streamInput()` 对齐

Claude Agent SDK 的 `streamInput` 允许向 `Query` 注入异步消息流（实现"连续对话"而无需轮询）。

Recursive SDK 对应实现：

```python
# 等价的 stream_input 模式
async def chat_loop(agent: AgentSession):
    async for user_msg in user_message_stream():
        run = agent.send(user_msg)
        async for event in run.stream():
            yield event
```

由于 `agent.send()` 已经是异步的，且子进程常驻内存，效果与 `streamInput` 等价。

---

## 文件变动清单

### Rust（新增/修改）

| 文件 | 变动 |
|------|------|
| `src/main.rs` | 新增 `Daemon` 子命令 CLI 参数 |
| `src/daemon.rs` | *(新文件)* `DaemonRunner` 实现 |
| `src/event.rs` | 新增 `DaemonEvent::Ready / TurnFinished / Closed` |
| `src/runtime.rs` | 暴露 `run_turn()` 方法供 DaemonRunner 调用 |

### Python SDK

| 文件 | 变动 |
|------|------|
| `sdk/python/recursive_sdk/transport.py` | *(新)* `Transport` protocol + `SubprocessTransport` + `HttpTransport` |
| `sdk/python/recursive_sdk/agent.py` | 重构：transport 注入替代硬编码 HTTP |
| `sdk/python/recursive_sdk/run.py` | transport 无关化（已接近，微调） |
| `sdk/python/tests/test_subprocess.py` | *(新)* subprocess transport 集成测试 |

### TypeScript SDK

| 文件 | 变动 |
|------|------|
| `sdk/typescript/src/transport.ts` | *(新)* `Transport` interface + `SubprocessTransport` + `HttpTransport` |
| `sdk/typescript/src/agent.ts` | transport 注入 |
| `sdk/typescript/tests/subprocess.test.ts` | *(新)* subprocess transport 测试 |

---

## 验收标准

1. `cargo build` 绿
2. `cargo test --workspace` 绿（含新增 daemon 协议单测）
3. `cargo clippy --all-targets -- -D warnings` 无警告
4. `cargo fmt --all` 无 diff
5. Python SDK：`python -m pytest sdk/python/tests/` 全通过
6. TypeScript SDK：`npm test` in `sdk/typescript/` 全通过
7. **端到端验证**：
   ```python
   import asyncio
   from recursive_sdk import Agent

   async def main():
       async with Agent.create() as agent:
           run = agent.send("list files in current dir")
           result = await run.wait()
           assert result.status == "finished"
           assert result.result  # 非空文字结果

   asyncio.run(main())
   ```
   无需预先启动任何 server。
8. HTTP 兼容性：`Agent.create(base_url="http://localhost:8080")` 仍然正常工作

---

## 分阶段实现计划

### Week 1 — Rust daemon 模式

- [ ] `src/daemon.rs` DaemonRunner 骨架
- [ ] stdin JSON 读取循环
- [ ] 调用 `AgentRuntime::run_turn()`，stdout 输出 events
- [ ] `turn_finished` / `ready` / `closed` 事件
- [ ] `interrupt` 信号处理（取消当前 turn）
- [ ] `src/main.rs` 接入 `daemon` 子命令
- [ ] 单元测试：协议解析 / 事件序列化

### Week 2 — Rust 集成 + 稳定化

- [ ] session resume（`--session-id`）
- [ ] stdin EOF 优雅退出
- [ ] 错误时输出 `closed { reason: "error" }` 再退出
- [ ] e2e 测试（`tests/daemon_smoke.rs`）
- [ ] clippy / fmt 全绿

### Week 3 — SDK 重写

- [ ] Python `SubprocessTransport` + `HttpTransport`
- [ ] `Agent` 重构（transport 注入）
- [ ] TypeScript 同步
- [ ] 更新 README（新的零配置示例）
- [ ] 全量测试通过

---

## 关联文档

- [SDK Gap 分析](sdk-gap-analysis-and-roadmap.md) — 与 Claude Agent SDK 的差距分析
- [Agent Run Kernel Architecture](agent-run-kernel-architecture.md) — 运行时架构不变量
- `@anthropic-ai/claude-agent-sdk` v0.1.77 `runtimeTypes.d.ts` — 参考接口定义

---

*提案生成时间：2026-06-01*
