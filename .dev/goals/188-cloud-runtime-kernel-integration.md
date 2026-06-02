# Goal 188 — CloudRuntime: AgentKernel 集成 StorageBackend + ToolSetProvider

**Roadmap**: Phase 21.4 — CloudRuntime（4/4）

**依赖**: Goal 181、183 合并后开始；Goal 182、185、186、187 可先完成或并行

**Design principle check**:
- 修改 `AgentKernelBuilder`，允许注入 `StorageBackend` + `SessionStore` + `ToolSetProvider`
- ❌ 不在 `agent.rs::Agent::run` 主循环内增加分支
- 向后兼容：不传这些参数时，默认使用 `LocalStorageBackend` + `NoopSessionStore` + `LocalToolSetProvider`

## Why

Goal 181-187 定义了 trait 并实现了各个 backend，但 `AgentKernel` 本身还不使用它们。
本 Goal 完成最后一块：将 trait 注入 `AgentKernelBuilder`，让 `AgentKernel` 
通过 `StorageBackend` 读写 transcript，通过 `SessionStore` checkpoint 热状态，
通过 `ToolSetProvider` 获取工具集。

完成后，只需在启动时注入不同的 backend 实现，就能切换本地/云端模式，零代码改动。

## Scope

### 1. 修改 `src/kernel.rs` — `AgentKernelBuilder` 接受 backend 注入

在 `AgentKernelBuilder` 中增加可选字段（使用动态分发，避免泛型爆炸）：

```rust
use crate::storage::{SessionStore, StorageBackend};
use crate::tool_set_provider::ToolSetProvider;
use std::sync::Arc;

pub struct AgentKernelBuilder {
    // ... 现有字段 ...
    
    /// Pluggable storage backend (default: LocalStorageBackend)
    storage: Option<Arc<dyn StorageBackend>>,
    /// Pluggable session state store (default: NoopSessionStore)
    session_store: Option<Arc<dyn SessionStore>>,
    /// Pluggable tool set provider (default: LocalToolSetProvider)
    tool_set_provider: Option<Arc<dyn ToolSetProvider>>,
}

impl AgentKernelBuilder {
    pub fn with_storage(mut self, backend: Arc<dyn StorageBackend>) -> Self {
        self.storage = Some(backend);
        self
    }
    
    pub fn with_session_store(mut self, store: Arc<dyn SessionStore>) -> Self {
        self.session_store = Some(store);
        self
    }
    
    pub fn with_tool_set_provider(mut self, provider: Arc<dyn ToolSetProvider>) -> Self {
        self.tool_set_provider = Some(provider);
        self
    }
}
```

`AgentKernel` 构建时，若未注入则使用默认值：

```rust
let storage = builder.storage.unwrap_or_else(|| {
    Arc::new(crate::storage::local::LocalStorageBackend::new(config.workspace.clone()))
});
let session_store = builder.session_store.unwrap_or_else(|| {
    Arc::new(crate::storage::NoopSessionStore)
});
let tool_provider = builder.tool_set_provider.unwrap_or_else(|| {
    Arc::new(crate::tool_set_provider::LocalToolSetProvider)
});
```

### 2. 在 `AgentKernel::run_turn` 中集成 checkpoint

在每个工具调用完成后，调用 `session_store.save_state()`：

```rust
// Inside Agent Loop, after each tool call:
let checkpoint = AgentCheckpointState {
    step: current_step,
    transcript_len: transcript.len(),
};
// Intentionally fire-and-forget: checkpoint failure is non-fatal
let _ = self.session_store.save_state(&self.session_id, &checkpoint).await;
```

### 3. 在 `AgentKernel` 启动时使用 `ToolSetProvider` 获取工具

```rust
// In AgentKernel::new or build():
let registry = self.tool_provider.build_registry();
```

取代原有的硬编码 `build_tool_registry()` 调用。

### 4. 测试

- `local_backend_integration`: 使用 `LocalStorageBackend` + `NoopSessionStore` + `LocalToolSetProvider` 跑一个简单的 mock agent，验证 transcript 被持久化到临时目录
- `noop_session_store_does_not_block_completion`: checkpoint 失败不影响 agent 完成
- `tool_set_provider_builds_registry`: `LocalToolSetProvider` 构建的 registry 非空

## Acceptance

- `cargo test --workspace` 绿色（含新集成测试）
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `AgentKernelBuilder` 有 `with_storage`、`with_session_store`、`with_tool_set_provider` 方法
- 默认行为与现有测试完全兼容（本地文件存储，无 checkpoint）
- `AgentKernel` 从 `ToolSetProvider` 获取工具注册表

## 集成示例（不在 Goal scope 内，供参考）

未来 CloudRuntime 启动：

```rust
// 云端多租户启动示例
let kernel = AgentKernelBuilder::new(config)
    .with_storage(Arc::new(S3StorageBackend::new(bucket, prefix, tenant_id).await?))
    .with_session_store(Arc::new(RedisSessionStore::new(&redis_url, Duration::from_secs(7200), "recursive")?))
    .with_tool_set_provider(Arc::new(DockerToolSetProvider { image: "ubuntu:22.04".into(), workspace }))
    .build()?;
```

本地单机启动（无变化，零配置）：

```rust
let kernel = AgentKernelBuilder::new(config).build()?;
```

## Notes for the agent

- 读 `src/kernel.rs` 了解 `AgentKernelBuilder` 和 `AgentKernel` 的现有结构。
- `Arc<dyn Trait>` 动态分发，避免泛型传染整个调用链。
- Checkpoint 调用设计为 fire-and-forget：`let _ = session_store.save_state(...).await;`
  ——失败不阻塞 Agent Loop，最多丢失最后一步的断点恢复信息。
- `build_tool_registry()` 的提取在 Goal 183 中已完成，本 Goal 直接使用。
- **不要改变 `agent.rs::Agent::run` 的核心逻辑**，只在 kernel 的 setup 路径上做注入。
