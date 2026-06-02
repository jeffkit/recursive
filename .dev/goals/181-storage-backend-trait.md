# Goal 181 — 存储后端 Trait 抽象层（StorageBackend / SessionStore）

**Roadmap**: Phase 20.1 — 存算分离基础架构（1/4）

**Design principle check**:
- 新增 `src/storage.rs` 模块，定义 trait
- ❌ 不修改 `agent.rs::Agent::run` 主循环
- 正交性：storage 层不依赖 LLM 内部；AgentKernel 通过 trait 依赖存储

## Why

Recursive 的长期目标是同时支持本地单机模式和云端多租户模式。两者的核心差异在于**存储**：
- 本地模式：transcript / memory / session state 存在本地文件系统
- 云端模式：需要外移到 Redis / Postgres / S3

引入 `StorageBackend` 和 `SessionStore` trait，让存储实现可插拔，同时不破坏任何现有功能。
这是存算分离的基础一步。

## Scope（精确做这些，不多不少）

### 1. 新建 `src/storage.rs`

定义两个核心 trait：

```rust
//! Storage abstraction layer for Recursive.
//!
//! Defines the traits that decouple the agent kernel from specific
//! storage backends (local filesystem, Redis, S3, etc.).

use crate::error::Result;
use crate::message::Message;
use std::future::Future;

/// Persistent storage for session transcript and memory entries.
///
/// The local implementation writes JSONL files under the workspace.
/// Cloud implementations can write to Redis, Postgres, or S3.
pub trait StorageBackend: Send + Sync + 'static {
    /// Load the full transcript for a session.
    fn load_transcript(
        &self,
        session_id: &str,
    ) -> impl Future<Output = Result<Vec<Message>>> + Send;

    /// Persist the full transcript for a session.
    fn save_transcript(
        &self,
        session_id: &str,
        messages: &[Message],
    ) -> impl Future<Output = Result<()>> + Send;

    /// Load a named memory entry (e.g. "user.md", "project.md").
    fn load_memory(
        &self,
        key: &str,
    ) -> impl Future<Output = Result<Option<String>>> + Send;

    /// Store a named memory entry.
    fn save_memory(
        &self,
        key: &str,
        value: &str,
    ) -> impl Future<Output = Result<()>> + Send;
}

/// Opaque snapshot of in-flight agent state for crash recovery / migration.
///
/// Kept intentionally minimal: only what's needed to resume after a
/// pod restart or failover.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentCheckpointState {
    /// Current step index inside the Agent Loop.
    pub step: usize,
    /// Last committed transcript length (number of messages).
    pub transcript_len: usize,
}

/// Hot-state store for in-flight Agent Loop checkpoints.
///
/// The local implementation is a no-op (memory only, no crash recovery).
/// Cloud implementations write to Redis with a short TTL.
pub trait SessionStore: Send + Sync + 'static {
    /// Persist the current loop state.
    fn save_state(
        &self,
        session_id: &str,
        state: &AgentCheckpointState,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Load the most recent checkpoint, or `None` if absent.
    fn load_state(
        &self,
        session_id: &str,
    ) -> impl Future<Output = Result<Option<AgentCheckpointState>>> + Send;

    /// Remove all checkpoint state for a session (cleanup after finish).
    fn delete_state(
        &self,
        session_id: &str,
    ) -> impl Future<Output = Result<()>> + Send;
}
```

### 2. 实现 `NoopSessionStore`（零开销，单机模式使用）

```rust
/// In-memory no-op SessionStore. No persistence; used in local mode.
pub struct NoopSessionStore;

impl SessionStore for NoopSessionStore {
    async fn save_state(&self, _id: &str, _state: &AgentCheckpointState) -> Result<()> {
        Ok(())
    }
    async fn load_state(&self, _id: &str) -> Result<Option<AgentCheckpointState>> {
        Ok(None)
    }
    async fn delete_state(&self, _id: &str) -> Result<()> {
        Ok(())
    }
}
```

### 3. `src/lib.rs` — 导出新模块

```rust
pub mod storage;
pub use storage::{AgentCheckpointState, NoopSessionStore, SessionStore, StorageBackend};
```

### 4. 错误变体（`src/error.rs`）

如果还没有 `Storage` 变体，添加：

```rust
/// Storage backend error (I/O, serialization, etc.)
Storage { message: String },
```

### 5. 测试

在 `src/storage.rs` 底部的 `#[cfg(test)] mod tests` 中：

- `noop_session_store_is_always_empty`: `load_state` 始终返回 `None`
- `noop_session_store_save_and_delete_are_noop`: `save_state` → `delete_state` → `load_state` 仍返回 `None`
- `agent_checkpoint_state_serializes`: `serde_json` 往返测试

## Acceptance

- `cargo test --workspace` 绿色
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `cargo fmt --all`
- `src/storage.rs` 存在并被 `lib.rs` 导出
- `NoopSessionStore` 实现了 `SessionStore` trait

## Notes for the agent

- 读 `src/error.rs` 检查是否已有 `Storage` 变体，若有则复用。
- 读 `src/session.rs` 和 `src/transcript.rs` 了解当前存储实现，**但本 goal 只定义 trait，不修改现有实现**。
- `impl Trait` 在 trait 方法中需要 `async fn` 或 `impl Future + Send`；用 `async fn` 最简洁（RPITIT，Rust 1.75+），但若 `Cargo.toml` 的 MSRV 不支持，改用 `BoxFuture`。
- **不要修改 `src/agent.rs` 或任何工具文件。**
- **不要** 实现 `LocalStorageBackend`，那是 Goal 182 的工作。
