# Goal 396 — HTTP 会话持久化：写路径接通（`AgentRuntimeBuilder` 注入 + 驱逐/关闭时落盘）

**Roadmap**: Phase 14 Persistence & State（Critical）。今天 `recursive http` 的会话
**从不落盘**，进程重启或 TTL 驱逐 = 静默丢弃整段对话。

**依赖**: Goal 395（落盘钩子挂在宿主层的驱逐/关闭路径上）。

**Design principle check**:
- Implemented as: 给 `AgentRuntimeBuilder` 加 storage / session_store 转发（对齐已有
  `compactor` 转发模式），在宿主层 close/evict 时调用
  `StorageBackend::save_transcript(session_id, &messages)`；默认 `LocalStorageBackend`。
- ❌ Does NOT 实现 Redis/S3 后端（本 goal 只做 Local + 接线点，cloud 档见「非目标」）。
- ❌ Does NOT 改 `RunCore` / 每轮热路径（不在每轮 fsync）。
- ❌ Does NOT 声称「崩溃零丢失」——语义是「最多丢最后一次落盘之后的轮次」，必须写进文档。

## Why（2026-09-27 核实）

1. **HTTP 从不写盘（实测）**：向一个会话发 600 轮后，`$RECURSIVE_HOME/workspaces/<hash>/`
   下只有一个 `path.txt`，`sessions/` 为空。
2. **builder 根本没有注入口**：`AgentRuntimeBuilder`（`src/runtime/builder.rs`）没有
   storage / session_store setter；只有 `AgentKernelBuilder::with_storage`
   （`src/kernel.rs:499`）和 `with_session_store`（`:507`）存在，而全仓**唯一**调用点在
   `tests/v060_storage_integration.rs:149-150`。
3. **默认是空实现**：`session_store` 默认 `NoopSessionStore`（`src/storage/mod.rs:142`、
   `src/kernel.rs:553-555`）。
4. **文档与现实不符**：
   - `src/http/handlers.rs:290-291`：注释称「transcript is auto-saved to the storage
     backend after each turn」——不成立。
   - `src/http/mod.rs:1052`：注释称驱逐前会保存——不成立。
   - `README.md:316-357` 的 cloud 表承诺 `RECURSIVE_REDIS_URL` / `RECURSIVE_S3_BUCKET`
     与「Stateless HTTP pods + shared Redis/S3」「Automatic via `restore_from_storage()`」
     —— 这些 env 全仓无读取点，`restore_from_storage` 函数**不存在**。
5. `AgentRuntime::close()`（`src/runtime.rs:324-336`）目前只 dispatch hook + 置位 `closed`
   ——它是落盘的天然挂钩点，但**不要在 `runtime.rs` 里加持久化逻辑**（line budget +
   职责问题）：由宿主层在 close 之后调用 backend。

## Scope（do exactly this, no more）

### 1. builder 注入口（`src/runtime/builder.rs`）

```rust
pub fn storage(mut self, storage: Arc<dyn crate::storage::StorageBackend>) -> Self;
pub fn session_store(mut self, store: Arc<dyn crate::storage::SessionStore>) -> Self;
```

两者都转发给 `self.kernel_builder`（方法已存在，**不需要改 `src/kernel.rs`**）。
同时在 `AgentRuntime` 上暴露**只读**取用以便宿主层落盘：

- 优先方案：`AgentRuntime` 已有 `transcript()` 访问器——先 `rg "pub fn transcript"` 确认；
  若存在则不加新 API。
- 若不存在且必须新增，先落 Goal 385（`runtime.rs` 剩 8 行 headroom），或把访问器放在
  `src/kernel.rs` 已有的转发方法旁（同样受 line budget 约束）。
  在 journal 里记录选择。

### 2. 存储后端的选择（HTTP 启动时，`crates/recursive-cli/src/main.rs`）

- 默认：`LocalStorageBackend::new(user_workspace_dir(&config.workspace)?)`（云存储目录，
  与 CLI 的 session 目录同级即可，写入 `<user_sessions_dir>/<session-id>/transcript.json`）。
- `cloud-runtime` feature 下：识别 `RECURSIVE_REDIS_URL` / `RECURSIVE_S3_BUCKET` **并构造**
  `RedisSessionStore` / `S3StorageBackend` 属于**另一个 goal**（见非目标）。本 goal 要求：
  这两个 env 被设置时**启动日志里明确打印「recognized but not yet wired in http mode」**
  （避免继续静默忽略），并保留后续接线点。

### 3. 写穿：宿主层在会话结束/驱逐时落盘

- 在 `SessionHost`（Goal 395）的 `remove`/`evict_idle`/`close_and_remove` 路径上调用
  `storage.save_transcript(id, runtime.transcript())`，**在锁外**（与 395 的锁范围要求一致）。
- 频率语义：**默认仅在 close/evict 落一次**（`save_transcript` 是全量覆盖，见
  `src/storage/mod.rs:63-66`）。不要每轮调用——那是 O(N²) 写入。
  可选：加一个可配的 `RECURSIVE_HTTP_TRANSCRIPT_FLUSH_SECS`（默认关闭/或 60s 防抖），
  用于「长会话被 kill 时少丢」；如果实现成本高，可在 journal 说明并留 TODO。
- 优雅关停（`with_graceful_shutdown` 后的清理）也要落盘：`rg "graceful_shutdown" src/http/mod.rs`
  找到关停路径，在那里对所有存活会话执行一次落盘。

### 4. 文档与注释对齐（本 goal 授权）

- 改写 `src/http/handlers.rs:290-291`、`src/http/mod.rs:1052` 的注释为事实描述。
- 更新 `README.md` 的 cloud cheatsheet：把「Automatic via `restore_from_storage()`」
  改为实际语义（本 goal 落地后：本地落盘 + 关闭/驱逐时保存；冷加载见 Goal 397；
  Redis/S3 未接线），或直接删除不存在的能力描述。

### 5. 测试（agent-presence / agent-mutants 门）

- 单测：`AgentRuntimeBuilder::storage(...)` 生效（用假 `StorageBackend` 记录调用）。
- 单测：宿主层 `remove`/`evict_idle` 触发一次 `save_transcript`，参数是**该会话的**
  transcript（两个会话互不串）。
- 集成测试（`tests/http.rs` 风格）：建会话 → 发 1 轮（`MockProvider`）→ 删除/驱逐 →
  断言写入内容可被 `load_transcript` 读回，且 **tool-call ↔ tool-result 配对**保持
  （invariant #8）。
- 假 backend 必须断言「落盘发生在锁外」（可与 Goal 395 的锁范围测试复用同一模式）。

## Files NOT to touch

- `src/kernel.rs`（除非 Goal 385 已落且确有必要；转发方法已存在）。
- `src/run_core.rs`、compaction 算法。
- Redis/S3 后端的实现与 `src/storage/{redis,s3}.rs` 的既有测试。
- `.dev/flows/`、`.flowcast/`。

## Acceptance

- `cargo test --workspace`、`cargo clippy --workspace --all-targets --all-features -- -D warnings`、
  `cargo fmt --all` 全绿。
- Grep: `rg "fn storage\(|fn session_store\(" src/runtime/builder.rs` 两个方法存在且转发。
- Grep: `rg "save_transcript" src/ crates/` 出现**生产调用点**（HTTP 宿主路径），
  不再只有 storage 实现与测试。
- 手工实测（journal 贴命令与结果）：`recursive http` 建会话 → 发一轮 → 等 TTL 驱逐/优雅
  关停 → 在 `$RECURSIVE_HOME` 下找到并可解析 transcript 文件。
- e2e 回归：`sh .dev/scripts/e2e-run.sh http-api`、`resume` 通过
  （注意 `RECURSIVE_SESSIONS_DIR` 是硬覆盖，会话断言按 `.dev/AGENTS.md` 的隔离规则写）。
- Journal: `.dev/journal/manual-20260927-goal396-http-persistence.md`。

## Notes for the agent (traps)

- **不要在 `AgentRuntime::close()` 里加落盘**：`close()` 是内核侧 API，且 `runtime.rs`
  处在 line-budget 边缘。落盘属于宿主层职责。
- **不要复用 `SessionPersistenceSink` 的每消息 fsync 路径**（`src/session/writer.rs:303-305`
  每条消息 `write_all` + `flush`，外加 `.meta.json` 的两次 fsync）。那是 CLI/TUI 的
  交互式会话语义；HTTP 多会话下它会变成每轮 N 次阻塞 fsync。
- **全量覆盖语义**：`save_transcript` 一次写整条 transcript，长会话（几 MB）会有一次
  明显写放大——这是有意的取舍（相对每轮 O(N²) 写入）。在 goal 的 journal 里写明。
- **Redis/S3 不是本 goal**：只做「启动时识别 env + 打日志」，不要顺手接半个后端
  （半接线的云存储比不接线更危险）。

