# Goal 395 — 抽出 `SessionHost`：会话注册表 + 准入 + TTL 驱逐（顺带修 reaper 的锁范围）

**Roadmap**: Phase 14 Persistence & State / Phase 17 Production Hardening。
前端公共逻辑下沉的第一步：会话生命周期今天只存在于 HTTP 接入层里。

**依赖**: 无。

**Design principle check**:
- Implemented as: 新文件 `src/session_host.rs`，把 `AppState` 里与 HTTP 传输无关的三件事
  （会话注册表、准入信号量、TTL 驱逐）搬进去；HTTP 改为持有 `Arc<SessionHost<..>>`。
- ❌ Does NOT 改 `AgentRuntime` / kernel / compaction（Goal 385 line budget）。
- ❌ Does NOT 改 REST 路由形状、鉴权、SSE 协议。
- ❌ Does NOT 引入新依赖（不引入 tokio 之外的并发原语）。

## Why（2026-09-27 核实）

会话生命周期逻辑今天散落在 `src/http/`：

| 职责 | 现在在哪 | 问题 |
|---|---|---|
| 会话表 | `src/http/mod.rs:304` `Arc<RwLock<HashMap<String, SessionState>>>` | 只有 HTTP 有；CLI/TUI/ACP 各写一遍 |
| 准入闸门 | `src/http/mod.rs:316` `run_semaphore`；三处 `handlers.rs:118/:895/:1620` | 只有 HTTP 有；且无等待上限（Goal 398） |
| TTL 驱逐 | `src/http/mod.rs:1054-1107` `spawn_session_reaper` | 见下 bug |
| 计数/指标 | 散落在 handler 各处 | Goal 392 才补齐 |

**已核实的 bug**：reaper 在**持有全局写锁期间** `.await` 每个会话的 `close()`：

```rust
// src/http/mod.rs:1082-1096
let mut sessions = state.sessions.write().await;      // 全服写锁
for id in &to_evict {
    if let Some(session) = sessions.remove(id) {
        if let Ok(mut rt) = session.runtime.try_lock() {
            rt.close(None).await;                     // ← 持写锁 await
        }
```

`close()` 一旦变成真实持久化（Goal 396 就是让它落盘），这里会变成「驱逐 N 个会话 =
阻塞全服会话 API 的时长 = N × 落盘时间」。`list_sessions` / `get_session` /
`send_session_message` / `fork_session` / `delete_session` 全部走 `read()`，会被一起卡住。

## Scope（do exactly this, no more）

### 1. 新文件 `src/session_host.rs`

定义与 HTTP 传输无关的宿主结构（泛型化的会话负载，避免 `session_host` 依赖 axum 类型）：

```rust
pub struct SessionHost<S> { /* sessions, run_semaphore, ttl, metrics hook */ }

impl<S> SessionHost<S> {
    pub fn new(ttl: Duration, max_concurrent_runs: usize) -> Self;
    pub async fn insert(&self, id: String, session: S);
    pub async fn get(&self, id: &str) -> Option<Arc<S>>;      // 或返回 clone 的句柄
    pub async fn remove(&self, id: &str) -> Option<S>;
    pub async fn len(&self) -> usize;
    pub async fn evict_idle<F, Fut>(&self, last_active: F, close: ...) -> Vec<String>;
    pub fn acquire_run(&self) -> ...;                          // 准入（Goal 398 细化）
}
```

要求：

- `evict_idle` **必须先把候选 id 收集到 `Vec` 并释放读锁，再在无锁状态下
  `remove` + `close().await`**：即 remove 用短写锁、close 在锁外。禁止在持有
  `sessions` 锁时 `await` 外部工作。
- 保留 `try_lock` 跳过忙会话的现有语义（忙会话不驱逐），并在文档与测试里写清。
- 驱逐时同步维护指标（`sessions_active` 递减），指标回调通过参数/闭包注入，
  不直接在 `session_host` 里引用 `http::Metrics`。

### 2. HTTP 改为使用它

- `AppState` 改为持有 `Arc<SessionHost<SessionState>>`；为了控制 diff 规模，可以保留
  `sessions` 访问器薄包装（例如 `state.host.sessions()`），使 `handlers.rs` 的既有
  调用点尽量少改。**不要为了「架构漂亮」顺手重写 handlers**——diff 越小越安全。
- `spawn_session_reaper` 保留签名，内部改为调用 `SessionHost::evict_idle`。

### 3. 测试（agent-presence / agent-mutants 门）

- **锁范围回归测试（本 goal 的核心）**：构造一个 `close` 会 `tokio::time::sleep(200ms)`
  的会话，`evict_idle` 在跑时，另一任务必须能在 < 50ms 内成功完成 `get()`/`len()`
  （证明读路径不被阻塞）。这条测试是 reaper bug 的 pin。
- TTL 语义：未超时的不驱逐、超时的驱逐、忙会话（`try_lock` 失败）跳过。
- 并发 insert/get/remove 不丢会话（简单 stress：1000 次并发操作后 len 正确）。
- 指标递减正确（用一个假计数器）。

## Files NOT to touch

- `src/runtime.rs`、`src/kernel.rs`、`src/run_core.rs`（内核与 line budget）。
- `src/http/handlers.rs` 的业务逻辑（除必要的调用点替换）。
- SSE `event_channels` 的迁移**不在本 goal**（它是传输层概念，留在 HTTP；
  若搬迁成本低也可顺手，但不得扩大 diff 到影响上述测试）。
- `src/session/**`（磁盘会话 = CLI 路径，另有语义）。

## Acceptance

- `cargo test --workspace`、`cargo clippy --workspace --all-targets --all-features -- -D warnings`、
  `cargo fmt --all` 全绿。
- 新测试按名可跑：`cargo test --lib session_host`（含锁范围回归用例）。
- 锁范围：`spawn_session_reaper` 不再在 `sessions.write()` 的 guard 作用域内 `await`
  外部工作（`close()` 必须在 guard drop 之后）；由上面那条「驱逐不阻塞读」的测试
  + journal 里的代码片段共同证明（不靠单一 grep 断言）。
- e2e 回归：`sh .dev/scripts/e2e-run.sh http-api` 与 `http-interrupt` 通过。
- Journal: `.dev/journal/manual-20260927-goal395-session-host.md`，记录搬迁清单与锁范围前后的
  实测（可贴测试耗时）。

## Notes for the agent (traps)

- **不要试图一次迁完所有东西**：本 goal 只搬「会话表 + 准入 + TTL 驱逐」。`event_channels`、
  鉴权、SSE、rate limiter 都留在 HTTP。
- 泛型 `S` 的目的是让 CLI/TUI 未来复用；但**本 goal 不迁移 CLI/TUI**，不要改它们。
  在 doc comment 里写清「这是给所有前端共用的宿主，目前只有 HTTP 接入」。
- `try_lock` 跳过忙会话会让被跳过的会话**留在表里**（不是移除后再丢），保持这个语义：
  丢会话比多留一轮更糟。
- 驱逐顺序：先 remove 再 close，close 失败不能让会话「复活」——close 的错误只记日志
  （与现状一致），不要 `?` 冒泡中断整轮驱逐。

