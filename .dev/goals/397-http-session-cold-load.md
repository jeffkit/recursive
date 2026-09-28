# Goal 397 — HTTP 会话冷加载：重启后从存储恢复会话（读路径）

**Roadmap**: Phase 14 Persistence & State。Goal 396 让会话能落盘；本 goal 让它们能被读回来
——否则「持久化」对用户不可见（重启后 `GET /sessions` 依然是空的）。

**依赖**: Goal 396（先有写路径）、Goal 394（恢复时必须用会话级隔离的 registry）。

**Design principle check**:
- Implemented as: 在 HTTP 的会话查找路径上增加「未命中内存 → `load_transcript` → 重建
  `AgentRuntime`（`seed_transcript`）→ 入表」的懒加载；复用宿主层（Goal 395）的插入接口。
- ❌ Does NOT 改 `RunCore` / kernel / compaction。
- ❌ Does NOT 改磁盘会话格式（`src/session/**` 是 CLI 路径，本 goal 只走
  `StorageBackend::load_transcript`）。
- ❌ Does NOT 承诺恢复「进行中的 turn」——只恢复 transcript（turn 边界语义）。

## Why（2026-09-27 核实）

- `StorageBackend` 已有读接口：`load_transcript(&self, session_id) -> Result<Vec<Message>>`，
  约定「不存在时返回空 Vec 而非错误」（`src/storage/mod.rs:52-61`）。
- `AgentRuntimeBuilder::seed_transcript(messages)` 已存在（`src/runtime/builder.rs:202`），
  语义是「插到 system prompt 之后、其余消息之前」。
- HTTP 当前行为：会话不在 `state.sessions` 里时，`GET /sessions/:id` 返回 404
  （`src/http/handlers.rs:390` 附近），`POST /sessions/:id/messages` 返回 404
  （`:865-870`）——重启后旧会话**在 API 上不存在**，即使 Goal 396 已经把文件写好。

## Scope（do exactly this, no more）

### 1. 懒加载入口

在 HTTP 的会话获取处抽一个统一入口（例如 `async fn get_or_load_session(state, id) -> Result<Arc<SessionState>>`），
被 `GET /sessions/:id`、`POST /sessions/:id/messages`、`POST /sessions/:id/interrupt`（只读路径可跳过）
使用：

1. 命中内存 → 直接返回。
2. 未命中 → `storage.load_transcript(id)`：
   - 空 Vec → 维持 404（不产生幽灵会话）。
   - 非空 → 用**当前配置**构建 runtime（必须复用 Goal 393 的上下文管理 helper 与
     Goal 394 的 `fork_session()`），`seed_transcript(msgs)`，`set_session_id(id)`，
     插入宿主表，更新 `sessions_active` 指标，返回。

### 2. 恢复时的 transcript 规范化（本 goal 最易出错处）

必须显式处理，并在测试里 pin：

- **剥掉存储里的 system 消息**：运行时自己会构建 system prompt；`seed_transcript` 会把它
  插在 system 之后。若直接 seed 一条含 system 的 transcript，会产生**两条 system**。
  规则：若 `msgs[0].role == System`，丢弃这一条。
- **保证 tool-call ↔ tool-result 配对（invariant #8）**：丢弃开头连续的 `Role::Tool`
  消息（孤儿 tool result），直到第一条不是 Tool 的消息为止。参考 invariant 测试
  `tests/invariants/tool_call_pairing.rs` 与 compaction 的 retreat 逻辑
  （`keep_recent_n` 的 split 回退）。
- **空 transcript 判定在规范化之后**：如果剥完只剩空，返回 404。

### 3. 会话元数据（最小实现）

`SessionState` 有 `created_at` / `title`（`src/http/mod.rs:71-100`）。
本 goal 的**最小语义**：冷加载时 `created_at` 用「文件 mtime 或当前时间」合成、
`title = None`，并在 doc comment 里写明「会话元数据的持久化不在本 goal」。
若实现成本低，可用 `StorageBackend::save_memory("session-meta/<id>", json)` 顺带持久化
`created_at`/`title`（`src/storage/mod.rs:72-78` 已有该接口）——**可选**，做了要在
journal 说明 key 约定。

### 4. 测试（agent-presence / agent-mutants 门）

- 单测/集成：落盘 → 清空宿主表（模拟重启）→ `GET /sessions/:id` 能取回；再发一轮
  `MockProvider` 对话，transcript 继续增长且**配对合法**。
- 单测：存储里含 system 消息时恢复后只有一条 system。
- 单测：存储里以孤儿 `Role::Tool` 开头时被剥掉，且不返回 400（配对测试）。
- 单测：`load_transcript` 返回空 → 404（不创建幽灵会话）。
- 不存在的 id → 404（现状语义不回退）。

## Files NOT to touch

- `src/session/**`（磁盘会话/CLI 路径）、`src/transcript.rs`。
- `src/run_core.rs`、`src/kernel.rs`、`src/runtime.rs`。
- `src/storage/{local,redis,s3}.rs` 的实现与其测试（本 goal 只用 trait）。

## Acceptance

- `cargo test --workspace`、`cargo clippy --workspace --all-targets --all-features -- -D warnings`、
  `cargo fmt --all` 全绿。
- `cargo test --test tool_call_pairing` 绿（invariant #8 回归）。
- 新测试按名可跑：`cargo test --lib cold_load` 或既有 `tests/http.rs` 的新用例
  （在 journal 里写清测试名与文件）。
- 手工实测（journal 贴命令）：建会话 → 发 1 轮 → 重启 server → `GET /sessions/:id`
  返回原 transcript（`message_count` > 0）。
- e2e 回归：`sh .dev/scripts/e2e-run.sh http-api`、`resume` 通过。
- Journal: `.dev/journal/manual-20260927-goal397-http-cold-load.md`。

## Notes for the agent (traps)

- **两条 system / 孤儿 tool 是本 goal 的两个真实雷**：任何「直接 seed 存储内容」的实现
  都会踩。先写规范化函数 + 单测，再接 HTTP。
- **不要用 `RECURSIVE_SESSIONS_DIR` 之外的路径猜文件位置**：本 goal 走
  `StorageBackend` trait，不直接读文件系统。手工实测时用 `RECURSIVE_HOME`/`RECURSIVE_SESSIONS_DIR`
  隔离，参考 `.dev/AGENTS.md` 的会话隔离规则。
- **冷加载必须用 `fork_session()`（Goal 394）**：否则恢复出来的会话会与现存会话共享
  read-before-edit 状态，等于把 394 修好的 bug 从后门放回来。
- 懒加载路径会持有宿主表锁——**不要在持锁期间 `load_transcript().await`**（那是 IO）。
  正确顺序：查表（短读锁）→ 释放 → IO → 再插入（短写锁，需处理并发双插：以先插入者为准）。

