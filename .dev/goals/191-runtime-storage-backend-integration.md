# Goal 191 — AgentRuntime: 使用 StorageBackend 持久化 transcript

**Roadmap**: Phase 22.1 — Runtime 存储标准化

**依赖**: Goal 188（AgentKernelBuilder 已集成 StorageBackend）

**Design principle check**:
- 修改 `AgentRuntime::run`，每轮结束后调用 `kernel.storage().save_transcript()`
- 新增 `restore_from_storage` 异步方法，从 storage 恢复 transcript
- 新增 `AgentRuntimeBuilder::with_storage` / `with_session_store` 方法（转发给 kernel_builder）
- ❌ 不改变 `SessionWriter`（其负责审计/JSONL 格式，存储后端 backend 负责 resume 恢复）
- ❌ 不在 agent.rs 主循环增加分支
- 向后兼容：未设置 `session_id` 时 save 是 no-op；默认 LocalStorageBackend 行为与现有一致

## Why

Goals 181–190 建立了可插拔的 `StorageBackend` 接口，但 `AgentRuntime` 的
transcript 仍由内存 Vec 持有，持久化由 `SessionWriter`（EventSink 外层）
在 `main.rs` 完成。这使得：
- HTTP 模式（`recursive serve`）无法在进程重启后恢复会话 transcript
- 云端注入 S3StorageBackend 后，transcript 仍写本地，无法跨 pod 恢复

本 Goal 让 `AgentRuntime` 通过 `kernel.storage()` 在每轮后自动持久化 transcript，
并支持从 storage 恢复。

## Scope

### 1. `AgentRuntime::run` — 每轮后 save_transcript

在 `self.turn_index += 1;` 之后添加：

```rust
// Persist transcript to storage backend (best-effort, logs warning on error).
if let Some(ref sid) = self.session_id {
    if let Err(e) = self.kernel.storage()
        .save_transcript(sid, &self.transcript)
        .await
    {
        tracing::warn!(session_id = %sid, error = %e, "storage: save_transcript failed");
    }
}
```

### 2. 新增 `AgentRuntime::restore_from_storage`

```rust
/// Load transcript from the kernel's storage backend for `session_id`.
///
/// Returns `Ok(true)` if messages were found and loaded, `Ok(false)` if the
/// session was not found (empty transcript returned by backend).
///
/// Also sets `self.session_id` so subsequent turns persist to the same key.
pub async fn restore_from_storage(&mut self, session_id: impl Into<String>) -> Result<bool> {
    let sid = session_id.into();
    let messages = self.kernel.storage().load_transcript(&sid).await?;
    if messages.is_empty() {
        self.session_id = Some(sid);
        return Ok(false);
    }
    self.transcript = messages;
    self.session_id = Some(sid);
    Ok(true)
}
```

### 3. `AgentRuntimeBuilder` — 新增 storage/session_store 方法

```rust
pub fn with_storage(mut self, backend: Arc<dyn StorageBackend>) -> Self {
    self.kernel_builder = self.kernel_builder.with_storage(backend);
    self
}

pub fn with_session_store(mut self, store: Arc<dyn SessionStore>) -> Self {
    self.kernel_builder = self.kernel_builder.with_session_store(store);
    self
}

pub fn with_tool_set_provider(mut self, provider: Arc<dyn ToolSetProvider>) -> Self {
    self.kernel_builder = self.kernel_builder.with_tool_set_provider(provider);
    self
}
```

### 4. 更新 `AgentRuntime::set_session_id` — 不自动触发 load

`set_session_id` 只设置 ID（不触发 storage 加载），加载需显式调用
`restore_from_storage`。这符合现有 CLI 流程（transcript 已从 SessionReader 加载）。

## 实现步骤

1. 修改 `src/runtime.rs`（3 处改动）
2. 更新 `src/lib.rs`（确保 `StorageBackend` 等已导出，已在 Goal 190 完成）
3. 在 `runtime.rs` 内部 tests 新增：
   - `storage_backend_saves_transcript_after_run`
   - `restore_from_storage_loads_transcript`

## 验收标准

- `cargo test --workspace` 全绿
- `cargo clippy -- -D warnings` 无警告
- HTTP 模式可通过 `set_session_id` + `restore_from_storage` 实现跨重启恢复
