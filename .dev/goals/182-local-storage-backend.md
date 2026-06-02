# Goal 182 — LocalStorageBackend：将现有文件存储包装为 StorageBackend 实现

**Roadmap**: Phase 20.2 — 存算分离基础架构（2/4）

**依赖**: Goal 181 合并后开始

**Design principle check**:
- 新增 `src/storage/local.rs`，实现 `StorageBackend` trait
- ❌ 不修改 `agent.rs` 主循环
- 正交性：LocalStorageBackend 是对现有 session/transcript 文件 IO 的薄封装

## Why

Goal 181 定义了 `StorageBackend` trait。本 Goal 实现第一个具体后端——
`LocalStorageBackend`，它将现有的本地 JSONL transcript 文件读写包装成 trait 实现。

完成后，现有的全部功能通过 `LocalStorageBackend` 运行，行为与原先完全一致。
同时为 CloudStorageBackend（Redis/S3，未来 Goal）提供了参考实现。

## Scope

### 1. 新建 `src/storage/` 目录，移动 trait 定义

将 Goal 181 新建的 `src/storage.rs` 改为目录：

```
src/storage/
  mod.rs       ← 原 storage.rs 内容（trait 定义）
  local.rs     ← 本 Goal 新增
```

在 `src/storage/mod.rs` 中 `pub mod local;` 并 re-export。

### 2. 实现 `LocalStorageBackend`

```rust
//! Local filesystem implementation of StorageBackend.

use std::path::PathBuf;
use crate::error::{Error, Result};
use crate::message::Message;
use crate::storage::StorageBackend;

/// Stores transcript in JSONL files and memory in markdown files,
/// both under the workspace directory — identical behavior to the
/// pre-trait implementation.
pub struct LocalStorageBackend {
    workspace: PathBuf,
}

impl LocalStorageBackend {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }

    fn transcript_path(&self, session_id: &str) -> PathBuf {
        self.workspace
            .join(".recursive")
            .join("sessions")
            .join(format!("{session_id}.jsonl"))
    }

    fn memory_path(&self, key: &str) -> PathBuf {
        self.workspace.join(".recursive").join("memory").join(key)
    }
}

impl StorageBackend for LocalStorageBackend {
    async fn load_transcript(&self, session_id: &str) -> Result<Vec<Message>> {
        let path = self.transcript_path(session_id);
        if !path.exists() {
            return Ok(vec![]);
        }
        let content = tokio::fs::read_to_string(&path).await.map_err(|e| {
            Error::Storage { message: format!("read transcript {path:?}: {e}") }
        })?;
        content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).map_err(|e| Error::Storage {
                message: format!("parse transcript line: {e}")
            }))
            .collect()
    }

    async fn save_transcript(&self, session_id: &str, messages: &[Message]) -> Result<()> {
        let path = self.transcript_path(session_id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| Error::Storage {
                message: format!("create dir {parent:?}: {e}")
            })?;
        }
        let content: String = messages
            .iter()
            .map(|m| serde_json::to_string(m).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");
        tokio::fs::write(&path, content).await.map_err(|e| Error::Storage {
            message: format!("write transcript {path:?}: {e}")
        })
    }

    async fn load_memory(&self, key: &str) -> Result<Option<String>> {
        let path = self.memory_path(key);
        if !path.exists() {
            return Ok(None);
        }
        tokio::fs::read_to_string(&path).await
            .map(Some)
            .map_err(|e| Error::Storage { message: format!("read memory {key}: {e}") })
    }

    async fn save_memory(&self, key: &str, value: &str) -> Result<()> {
        let path = self.memory_path(key);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| Error::Storage {
                message: format!("create dir {parent:?}: {e}")
            })?;
        }
        tokio::fs::write(&path, value).await
            .map_err(|e| Error::Storage { message: format!("write memory {key}: {e}") })
    }
}
```

### 3. 导出

在 `src/storage/mod.rs` 中：

```rust
pub mod local;
pub use local::LocalStorageBackend;
```

在 `src/lib.rs` 中确保 `LocalStorageBackend` 被 re-export。

### 4. 测试

在 `src/storage/local.rs` 的 `#[cfg(test)] mod tests` 中：

- `save_and_load_transcript_roundtrip`：写入若干 `Message`，读回应与原始相等
- `load_transcript_nonexistent_returns_empty`：session 不存在时返回空 vec
- `save_and_load_memory_roundtrip`：写入字符串，读回应与原始相等
- `load_memory_nonexistent_returns_none`：key 不存在时返回 `None`
- `save_transcript_creates_parent_dirs`：父目录不存在时自动创建

## Acceptance

- `cargo test --workspace` 绿色（含新测试）
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `LocalStorageBackend` 在 `lib.rs` 中被公开导出
- 所有现有功能（`cargo test` 中已有的 smoke test 等）继续通过

## Notes for the agent

- 读 `src/transcript.rs` 和 `src/session.rs` 了解现有 JSONL 格式，**保持格式兼容**。
- `Message` 需要 `serde::Serialize + serde::Deserialize`，应已满足。
- 本 Goal **不修改任何调用侧**；`LocalStorageBackend` 只是新增，暂时没有被 AgentKernel 调用。
- 调用侧集成是后续 Goal（Goal 184）的工作。
- 若发现现有 transcript 格式与 `serde_json::from_str::<Message>` 不兼容，记录在 journal 中，不要静默丢弃。
