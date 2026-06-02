# Goal 185 — CloudRuntime: RedisSessionStore 热状态持久化

**Roadmap**: Phase 21.1 — CloudRuntime（1/4）

**依赖**: Goal 181（StorageBackend trait）合并后开始

**Feature flag**: `cloud-runtime`（新增）

**Design principle check**:
- 新增 `src/storage/redis.rs`，实现 `SessionStore` trait
- feature-gated，不影响默认构建
- ❌ 不修改 `agent.rs` 主循环

## Why

云端多租户场景下，Agent Loop 的热状态（当前步骤、transcript 长度）必须存在进程外，
否则 pod 崩溃后无法恢复会话。Redis 是业界标准选择：TTL 自动清理、亚毫秒读写、
水平扩展友好。

本 Goal 实现 `RedisSessionStore`，通过 JSON 序列化将 `AgentCheckpointState` 
存入 Redis，支持 TTL 配置。

## Scope

### 1. `Cargo.toml` — 新增 `cloud-runtime` feature 和 redis 依赖

```toml
[features]
cloud-runtime = ["dep:redis", "dep:deadpool-redis"]

[dependencies]
redis = { version = "0.27", features = ["tokio-comp"], optional = true }
deadpool-redis = { version = "0.18", optional = true }
```

### 2. 新建 `src/storage/redis.rs`

```rust
//! Redis-backed SessionStore for cloud multi-tenant deployments.

use crate::error::{Error, Result};
use crate::storage::{AgentCheckpointState, SessionStore};
use deadpool_redis::{Config, Pool, Runtime};
use redis::AsyncCommands;
use std::time::Duration;

pub struct RedisSessionStore {
    pool: Pool,
    /// Key TTL — auto-expire sessions after inactivity (default: 2 hours)
    ttl: Duration,
    key_prefix: String,
}

impl RedisSessionStore {
    pub fn new(redis_url: &str, ttl: Duration, key_prefix: impl Into<String>) -> Result<Self> {
        let cfg = Config::from_url(redis_url);
        let pool = cfg.create_pool(Some(Runtime::Tokio1)).map_err(|e| Error::Storage {
            message: format!("create redis pool: {e}"),
        })?;
        Ok(Self { pool, ttl, key_prefix: key_prefix.into() })
    }

    fn key(&self, session_id: &str) -> String {
        format!("{}:session:{}", self.key_prefix, session_id)
    }
}

impl SessionStore for RedisSessionStore {
    async fn save_state(&self, session_id: &str, state: &AgentCheckpointState) -> Result<()> {
        let mut conn = self.pool.get().await.map_err(|e| Error::Storage {
            message: format!("redis pool get: {e}"),
        })?;
        let key = self.key(session_id);
        let value = serde_json::to_string(state).map_err(|e| Error::Storage {
            message: format!("serialize state: {e}"),
        })?;
        let ttl_secs = self.ttl.as_secs() as i64;
        conn.set_ex::<_, _, ()>(&key, value, ttl_secs as u64).await.map_err(|e| Error::Storage {
            message: format!("redis set_ex {key}: {e}"),
        })
    }

    async fn load_state(&self, session_id: &str) -> Result<Option<AgentCheckpointState>> {
        let mut conn = self.pool.get().await.map_err(|e| Error::Storage {
            message: format!("redis pool get: {e}"),
        })?;
        let key = self.key(session_id);
        let value: Option<String> = conn.get(&key).await.map_err(|e| Error::Storage {
            message: format!("redis get {key}: {e}"),
        })?;
        match value {
            None => Ok(None),
            Some(v) => {
                let state = serde_json::from_str(&v).map_err(|e| Error::Storage {
                    message: format!("deserialize state: {e}"),
                })?;
                Ok(Some(state))
            }
        }
    }

    async fn delete_state(&self, session_id: &str) -> Result<()> {
        let mut conn = self.pool.get().await.map_err(|e| Error::Storage {
            message: format!("redis pool get: {e}"),
        })?;
        let key = self.key(session_id);
        conn.del::<_, ()>(&key).await.map_err(|e| Error::Storage {
            message: format!("redis del {key}: {e}"),
        })
    }
}
```

### 3. 导出

`src/storage/mod.rs` 中：

```rust
#[cfg(feature = "cloud-runtime")]
pub mod redis;
#[cfg(feature = "cloud-runtime")]
pub use redis::RedisSessionStore;
```

### 4. 测试策略

- **单元测试**：仅在有 `RECURSIVE_TEST_REDIS_URL` 环境变量时运行，否则 `skip`
- 使用 `#[tokio::test]` + `tokio::time::timeout` 避免挂起
- 测试 save/load/delete 的完整 roundtrip

```rust
#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn redis_session_store_roundtrip() {
        let url = match std::env::var("RECURSIVE_TEST_REDIS_URL") {
            Ok(u) => u,
            Err(_) => return, // skip if no Redis available
        };
        // ... test body
    }
}
```

## Acceptance

- `cargo build --features cloud-runtime` 编译通过
- `cargo test --workspace` 绿色（无 Redis 时测试 skip，不 fail）
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `RedisSessionStore` 实现 `SessionStore` trait，并在 `lib.rs` 中 feature-gated 导出
- 现有无 feature 的构建不受影响

## Notes for the agent

- 读 `src/storage/mod.rs` 了解 Goal 181 定义的 trait 结构。
- `deadpool-redis` 提供连接池，避免每次创建连接。
- TTL 默认 7200 秒（2 小时），可通过构造函数参数覆盖。
- **不要** 把 Redis URL 硬编码；通过 `RECURSIVE_REDIS_URL` 环境变量读取。
- 测试必须有超时保护：`tokio::time::timeout(Duration::from_secs(5), ...)` 包裹所有 Redis 调用。
