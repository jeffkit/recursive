# Goal 186 — CloudRuntime: S3StorageBackend 对象存储后端

**Roadmap**: Phase 21.2 — CloudRuntime（2/4）

**依赖**: Goal 181（StorageBackend trait）合并后开始；与 Goal 185 可并行

**Feature flag**: `cloud-runtime`（复用 Goal 185 新增的 feature）

**Design principle check**:
- 新增 `src/storage/s3.rs`，实现 `StorageBackend` trait
- feature-gated，不影响默认构建
- ❌ 不修改 `agent.rs` 主循环

## Why

云端多租户场景下，transcript、memory、工作产物需要存在对象存储（S3/OSS），
而不是 pod 本地磁盘，以实现：
- Pod 无状态化（任意 pod 可处理任意会话）
- 多租户路径隔离（`{bucket}/{tenant_id}/{session_id}/...`）
- 自动生命周期管理（S3 lifecycle policy 清理旧 session）

`S3StorageBackend` 使用 `aws-sdk-s3` 实现 `StorageBackend` trait。

## Scope

### 1. `Cargo.toml` — 更新 `cloud-runtime` feature，增加 S3 依赖

```toml
cloud-runtime = [
    "dep:redis", "dep:deadpool-redis",
    "dep:aws-sdk-s3", "dep:aws-config",
]

[dependencies]
aws-sdk-s3 = { version = "1", optional = true }
aws-config = { version = "1", features = ["behavior-version-latest"], optional = true }
```

### 2. 新建 `src/storage/s3.rs`

```rust
//! S3-backed StorageBackend for cloud multi-tenant deployments.
//!
//! Objects are stored with keys:
//!   transcript: {prefix}/{tenant_id}/{session_id}/transcript.jsonl
//!   memory:     {prefix}/{tenant_id}/memory/{key}

use crate::error::{Error, Result};
use crate::message::Message;
use crate::storage::StorageBackend;
use aws_sdk_s3::Client;

pub struct S3StorageBackend {
    client: Client,
    bucket: String,
    /// Key prefix, e.g. "recursive/prod"
    prefix: String,
    /// Tenant identifier for namespace isolation
    tenant_id: String,
}

impl S3StorageBackend {
    pub async fn new(
        bucket: impl Into<String>,
        prefix: impl Into<String>,
        tenant_id: impl Into<String>,
    ) -> Result<Self> {
        let config = aws_config::load_from_env().await;
        let client = Client::new(&config);
        Ok(Self {
            client,
            bucket: bucket.into(),
            prefix: prefix.into(),
            tenant_id: tenant_id.into(),
        })
    }

    fn transcript_key(&self, session_id: &str) -> String {
        format!("{}/{}/{}/transcript.jsonl", self.prefix, self.tenant_id, session_id)
    }

    fn memory_key(&self, key: &str) -> String {
        format!("{}/{}/memory/{}", self.prefix, self.tenant_id, key)
    }
}

impl StorageBackend for S3StorageBackend {
    async fn load_transcript(&self, session_id: &str) -> Result<Vec<Message>> {
        let key = self.transcript_key(session_id);
        let resp = self.client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await;

        match resp {
            Err(e) if is_not_found(&e) => return Ok(vec![]),
            Err(e) => return Err(Error::Storage { message: format!("s3 get {key}: {e}") }),
            Ok(output) => {
                let bytes = output.body.collect().await
                    .map_err(|e| Error::Storage { message: format!("s3 read body: {e}") })?
                    .into_bytes();
                let content = String::from_utf8_lossy(&bytes);
                content.lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| serde_json::from_str(l).map_err(|e| Error::Storage {
                        message: format!("parse message: {e}")
                    }))
                    .collect()
            }
        }
    }

    async fn save_transcript(&self, session_id: &str, messages: &[Message]) -> Result<()> {
        let key = self.transcript_key(session_id);
        let content = messages.iter()
            .map(|m| serde_json::to_string(m).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(&key)
            .body(content.into_bytes().into())
            .send()
            .await
            .map_err(|e| Error::Storage { message: format!("s3 put {key}: {e}") })?;
        Ok(())
    }

    async fn load_memory(&self, key: &str) -> Result<Option<String>> {
        let s3_key = self.memory_key(key);
        let resp = self.client
            .get_object()
            .bucket(&self.bucket)
            .key(&s3_key)
            .send()
            .await;

        match resp {
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(Error::Storage { message: format!("s3 get {s3_key}: {e}") }),
            Ok(output) => {
                let bytes = output.body.collect().await
                    .map_err(|e| Error::Storage { message: format!("s3 read body: {e}") })?
                    .into_bytes();
                Ok(Some(String::from_utf8_lossy(&bytes).to_string()))
            }
        }
    }

    async fn save_memory(&self, key: &str, value: &str) -> Result<()> {
        let s3_key = self.memory_key(key);
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(&s3_key)
            .body(value.as_bytes().to_vec().into())
            .send()
            .await
            .map_err(|e| Error::Storage { message: format!("s3 put {s3_key}: {e}") })?;
        Ok(())
    }
}

fn is_not_found(e: &aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::get_object::GetObjectError>) -> bool {
    matches!(e, aws_sdk_s3::error::SdkError::ServiceError(se)
        if se.err().is_no_such_key())
}
```

### 3. 导出

`src/storage/mod.rs` 中：

```rust
#[cfg(feature = "cloud-runtime")]
pub mod s3;
#[cfg(feature = "cloud-runtime")]
pub use s3::S3StorageBackend;
```

### 4. 测试策略

- 单元测试仅在 `RECURSIVE_TEST_S3_BUCKET` + `RECURSIVE_TEST_S3_PREFIX` 环境变量存在时运行
- 测试 save/load/delete（通过 save 空内容模拟 delete）的 roundtrip

## Acceptance

- `cargo build --features cloud-runtime` 编译通过
- `cargo test --workspace` 绿色（无 S3 凭据时 skip）
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `S3StorageBackend` 实现 `StorageBackend`，并在 `lib.rs` 中 feature-gated 导出

## Notes for the agent

- 用 `aws-sdk-s3` v1（最新稳定版），不要用 rusoto（已 deprecated）。
- `is_not_found` 辅助函数的具体类型需根据 SDK 版本调整，编译报错时对照 SDK 文档修正。
- 多租户隔离靠 key prefix，不需要额外权限系统。
- 测试必须有超时保护，避免挂起。
