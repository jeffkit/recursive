# Proposal: Recursive 存算分离架构 — 兼容本地与云端多租户

> **Status**: Draft  
> **Created**: 2026-06-02  
> **Authors**: [对话总结：kongjie + Cursor Agent]  
> **Scope**: 架构演进方向 — 存算分离、沙盒化、多租户支持  

---

## 背景

Recursive 的设计目标是：**一套内核，同时支持本地单机 Agent 和云端并发多租户 Agent**。

当前状态（v0.6.x）是一个成熟的本地裸机 Agent：
- 所有存储在本地文件系统（session transcript、memory、facts）
- Agent Loop 运行在单进程，状态在进程内存中
- 工具沙盒仅靠路径限制（`tools::resolve_within`），`run_shell` 无 OS 级隔离
- 已有 `AgentKernel`/`AgentRuntime` 的良好抽象基础

---

## 设计目标

| 目标 | 描述 |
|------|------|
| 向后兼容 | 本地单机模式继续工作，不增加任何 overhead |
| 云端多租户 | 同一套内核，通过注入不同 backend 支持多租户高并发 |
| 工具沙盒化 | 本地模式：路径限制；云端模式：OS 级隔离（Firecracker/gVisor/Docker） |
| 存储可插拔 | 文件系统 → Redis/DB/S3 等外部存储，实现 pod 无状态化 |
| 零重复代码 | Agent Loop、重试逻辑、compaction、tool 调度等公共逻辑只写一份 |

---

## 核心架构：共享内核 + 分叉 Runtime

### 分层设计

```
┌──────────────────────────────────────────────────────┐
│              AgentKernel（不动，保持 tiny）            │
│  - Agent Loop（推理→工具→观测→再推理）               │
│  - Compaction                                        │
│  - Finish Reason / Budget                            │
│  - 只依赖 trait 抽象，不知道存储细节                  │
└─────────────────────┬────────────────────────────────┘
                       │ 注入 trait 实现
        ┌──────────────┴──────────────┐
        ▼                             ▼
┌──────────────────┐       ┌─────────────────────────┐
│  LocalRuntime    │       │  CloudRuntime            │
│                  │       │                          │
│ StorageBackend   │       │ StorageBackend           │
│  └ 本地文件系统  │       │  ├ Redis（热状态）        │
│                  │       │  ├ Postgres（会话历史）   │
│ ToolSetProvider  │       │  └ S3/OSS（工作产物）    │
│  └ 路径限制工具  │       │                          │
│                  │       │ ToolSetProvider          │
│ SessionStore     │       │  └ SandboxedToolProvider │
│  └ 本地 JSONL    │       │     (E2B/Docker/gVisor)  │
└──────────────────┘       │                          │
                           │ SessionStore             │
                           │  └ Redis + DB 持久化     │
                           └─────────────────────────┘
```

### 三个核心 Trait

```rust
/// 存储后端：读写 transcript、memory 等持久化数据
pub trait StorageBackend: Send + Sync {
    async fn load_transcript(&self, session_id: &str) -> Result<Vec<Message>>;
    async fn save_transcript(&self, session_id: &str, msgs: &[Message]) -> Result<()>;
    async fn load_memory(&self, key: &str) -> Result<Option<String>>;
    async fn save_memory(&self, key: &str, value: &str) -> Result<()>;
}

/// 工具集提供者：返回当前 runtime 的工具实现集合
pub trait ToolSetProvider: Send + Sync {
    fn tools(&self) -> ToolRegistry;
    fn sandbox_mode(&self) -> SandboxMode;
}

/// 会话状态存储：保存/恢复 Agent Loop 的热状态（step、plan、游标等）
pub trait SessionStore: Send + Sync {
    async fn save_state(&self, session_id: &str, state: &AgentState) -> Result<()>;
    async fn load_state(&self, session_id: &str) -> Result<Option<AgentState>>;
    async fn delete_state(&self, session_id: &str) -> Result<()>;
}
```

**AgentKernel 只依赖这三个 trait**，不关心底层是文件、Redis 还是 S3。

---

## 沙盒策略选型

### 三层沙盒强度

| 级别 | 技术 | 适用场景 | 启动时间 | 安全强度 |
|------|------|---------|---------|---------|
| **L0** | 路径限制（现状） | 本地开发、可信环境 | 0ms | 低 |
| **L1** | Policy-based（fake-cc 方案） | 受控多租户，工具调用频繁 | 0ms | 中 |
| **L2** | gVisor（userspace kernel） | 云端多租户 SaaS | <1s | 高 |
| **L3** | Firecracker/E2B（microVM） | 高安全需求，托管云平台 | ~125ms | 极高 |

### fake-cc 的 Policy-based 方案（值得参考）

Claude Code（fake-cc）使用 `@anthropic-ai/sandbox-runtime` 包，在工具执行层做策略封装：

```typescript
// fake-cc: BashTool 被 SandboxManager 包裹
import { SandboxManager } from '../../utils/sandbox/sandbox-adapter.js'

// SandboxManager 提供：
// - FsReadRestrictionConfig / FsWriteRestrictionConfig
// - NetworkRestrictionConfig（域名白名单）
// - SandboxViolationStore（违规记录）
// - SandboxAskCallback（用户确认回调）
```

**在 Recursive（Rust）中等价实现**：

```rust
/// 策略沙盒包装器：在工具执行前后做策略检查
pub struct PolicySandboxWrapper<T: Tool> {
    inner: T,
    fs_policy: FsPolicy,
    network_policy: NetworkPolicy,
    violation_store: Arc<ViolationStore>,
}

impl<T: Tool> Tool for PolicySandboxWrapper<T> {
    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<Value> {
        self.check_policy(&input)?;  // 执行前检查
        let result = self.inner.call(input, ctx).await?;
        self.record_usage(&result);  // 执行后记录
        Ok(result)
    }
}
```

这就是"工具替换"而不是"工具重写"——内层工具逻辑不变，外层加策略。

### 推荐方案：渐进式实现

```
v0.7: L1 Policy-based 沙盒（Rust 原生实现）
      → 立即提升安全性，无启动开销
      → 参考 fake-cc 的策略配置模型

v0.8: L2 Docker/gVisor（可选，通过 CloudRuntime 注入）
      → run_shell → Docker exec API
      → 读写工具 → 挂载 volume 实现

v1.0: L3 E2B/Firecracker（可选，高安全托管云）
      → 适合 Recursive-as-a-Service 场景
```

---

## 多租户支持：4 件事

### 1. Session 状态外移（最关键）

**现状**：transcript 和 Agent Loop 状态在进程内存。Pod 崩溃，状态丢失。

**目标**：
- `Transcript` → 持久化到 `StorageBackend`，每次工具调用后同步写
- `AgentState`（step、plan、游标）→ `SessionStore`，每步执行后 checkpoint

```
每个 Agent 步骤：
  1. LLM 调用 → 得到 tool_call
  2. SessionStore.save_state(state)    ← 这一行加在这里
  3. Tool 执行 → 得到 result
  4. StorageBackend.save_transcript(msgs)   ← 这一行加在这里
  5. 下一步
```

### 2. 多租户路径隔离

**现状**：`workspace` 是单用户本地目录。

**目标**：`StorageBackend` 实现时加 `tenant_id` 前缀：
```
本地: workspace/file.rs
云端: s3://recursive-data/{tenant_id}/{session_id}/file.rs
```

现有的 `tools::resolve_within` 可以直接拍平到这个 namespace 实现。

### 3. 权限系统动态化

**现状**：`PermissionsConfig` 是启动时加载的静态 TOML。

**目标**：增加 `PermissionStore trait`，允许 per-user 动态权限：
```rust
pub trait PermissionStore: Send + Sync {
    async fn load_permissions(&self, tenant_id: &str) -> Result<PermissionsConfig>;
}
```

本地实现：读 TOML 文件（现有逻辑不变）  
云端实现：从 DB 读取用户权限配置

### 4. 无状态化后，网关路由变简单

一旦 (1) 完成，pod 完全无状态。任意请求可路由到任意 pod，不需要 session affinity。  
这是最干净的负载均衡方案。

---

## 实施路径

### Phase 1 — 基础架构（不破坏现有功能，1-2 周）

**目标**：引入 trait 抽象，LocalRuntime 是第一个实现，功能等价于现在。

1. 定义 `StorageBackend`, `SessionStore`, `ToolSetProvider` trait
2. 将现有逻辑包装成 `LocalStorageBackend` / `LocalSessionStore`
3. `AgentKernel` 改为依赖 trait（DI 注入）
4. 所有现有测试继续通过

### Phase 2 — Policy 沙盒（L1，2 周）

1. 实现 `FsPolicy` / `NetworkPolicy` 规则引擎
2. `PolicySandboxWrapper<T>` 包装 `run_shell` 和 `fs` 工具
3. 本地和云端都可配置沙盒强度
4. 与 `PermissionsConfig` 整合

### Phase 3 — CloudRuntime（3-4 周）

1. `RedisSessionStore`：热状态持久化
2. `S3StorageBackend`：transcript/memory 外移
3. `DockerToolProvider`：`run_shell` 走 Docker exec
4. `CloudRuntime` 组合以上三者
5. HTTP Server 层完善，支持真正的多会话并发

### Phase 4 — E2B/Firecracker（按需）

1. `E2bToolProvider`：接入 E2B SDK
2. 沙盒池（预热），降低冷启动影响
3. 适用于 Recursive-as-a-Service 产品形态

---

## 对比：扩展 vs 平行 Runtime

| 方案 | 优点 | 缺点 |
|------|------|------|
| **单 Runtime，if/else 分支** | 简单 | 两种模式的差异全塞进 if，代码越来越臃肿 |
| **完全平行的新 Runtime** | 独立演进 | Agent Loop、重试、compaction 逻辑重复，两个 bug |
| **✅ 共享内核 + trait 分叉** | 公共逻辑零重复；各实现独立演进；测试清晰 | 需要设计好 trait 接口 |

选择第三种。`AgentKernel` 不变，`LocalRuntime` 和 `CloudRuntime` 通过 trait 注入不同 backend，互不影响。这正是 Recursive 现有 `Agent`（旧）→ `AgentKernel`（新）迁移方向的延续。

---

## 总结

| 维度 | 现状 | Phase 1 | Phase 3 |
|------|------|---------|---------|
| 存储 | 本地文件 | 本地文件（trait 封装） | Redis+DB+S3 |
| 沙盒 | 路径限制 | Policy-based（L1） | Docker/E2B（L2/L3） |
| 多租户 | 不支持 | 架构就绪 | 完整支持 |
| 水平扩展 | 不支持 | 不支持 | Pod 无状态，支持 |
| 本地兼容 | ✅ | ✅ | ✅ |

*提案生成时间：2026-06-02*  
*基于 kongjie & Cursor Agent 企微讨论整理*
