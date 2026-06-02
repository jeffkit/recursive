//! S3-backed [`StorageBackend`] for cloud multi-tenant deployments.
//!
//! Objects are stored under structured keys for per-tenant isolation:
//! - Transcripts: `{prefix}/{tenant_id}/{session_id}/transcript.jsonl`
//! - Memory:      `{prefix}/{tenant_id}/memory/{key}`
//!
//! Gated behind the `cloud-runtime` feature flag.

use aws_sdk_s3::Client;

use crate::error::{Error, Result};
use crate::message::Message;
use crate::storage::StorageBackend;

/// S3-backed implementation of [`StorageBackend`].
///
/// Uses the AWS SDK v1. Credentials and region are loaded from the environment
/// via `aws_config::load_from_env()` (honours `AWS_*` environment variables,
/// instance metadata, etc.).
pub struct S3StorageBackend {
    client: Client,
    bucket: String,
    /// Key namespace, e.g. `"recursive/prod"`.
    prefix: String,
    /// Tenant identifier used to isolate keys between users.
    tenant_id: String,
}

impl S3StorageBackend {
    /// Create a new backend. Credentials are read from the environment.
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

    /// Create from an existing AWS client (useful for testing with mocks).
    pub fn with_client(
        client: Client,
        bucket: impl Into<String>,
        prefix: impl Into<String>,
        tenant_id: impl Into<String>,
    ) -> Self {
        Self {
            client,
            bucket: bucket.into(),
            prefix: prefix.into(),
            tenant_id: tenant_id.into(),
        }
    }

    fn transcript_key(&self, session_id: &str) -> String {
        format!(
            "{}/{}/{}/transcript.jsonl",
            self.prefix, self.tenant_id, session_id
        )
    }

    fn memory_key(&self, key: &str) -> String {
        format!("{}/{}/memory/{}", self.prefix, self.tenant_id, key)
    }
}

impl StorageBackend for S3StorageBackend {
    async fn load_transcript(&self, session_id: &str) -> Result<Vec<Message>> {
        let key = self.transcript_key(session_id);
        let resp = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await;

        match resp {
            Err(e) if is_not_found(&e) => Ok(vec![]),
            Err(e) => Err(Error::Storage {
                message: format!("s3 GET {key}: {e}"),
            }),
            Ok(output) => {
                let bytes = output
                    .body
                    .collect()
                    .await
                    .map_err(|e| Error::Storage {
                        message: format!("s3 read body for {key}: {e}"),
                    })?
                    .into_bytes();
                let content = String::from_utf8_lossy(&bytes);
                content
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| {
                        serde_json::from_str(l).map_err(|e| Error::Storage {
                            message: format!("parse transcript line: {e}"),
                        })
                    })
                    .collect()
            }
        }
    }

    async fn save_transcript(&self, session_id: &str, messages: &[Message]) -> Result<()> {
        let key = self.transcript_key(session_id);
        let mut lines = Vec::with_capacity(messages.len());
        for m in messages {
            let line = serde_json::to_string(m).map_err(|e| Error::Storage {
                message: format!("serialize message: {e}"),
            })?;
            lines.push(line);
        }
        let body = lines.join("\n").into_bytes();
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(&key)
            .body(body.into())
            .send()
            .await
            .map_err(|e| Error::Storage {
                message: format!("s3 PUT {key}: {e}"),
            })?;
        Ok(())
    }

    async fn load_memory(&self, key: &str) -> Result<Option<String>> {
        let s3_key = self.memory_key(key);
        let resp = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&s3_key)
            .send()
            .await;

        match resp {
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(Error::Storage {
                message: format!("s3 GET {s3_key}: {e}"),
            }),
            Ok(output) => {
                let bytes = output
                    .body
                    .collect()
                    .await
                    .map_err(|e| Error::Storage {
                        message: format!("s3 read body for {s3_key}: {e}"),
                    })?
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
            .map_err(|e| Error::Storage {
                message: format!("s3 PUT {s3_key}: {e}"),
            })?;
        Ok(())
    }
}

/// Returns `true` when `e` represents an S3 "key not found" (NoSuchKey).
fn is_not_found(
    e: &aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::get_object::GetObjectError>,
) -> bool {
    matches!(
        e,
        aws_sdk_s3::error::SdkError::ServiceError(se) if se.err().is_no_such_key()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{timeout, Duration};

    async fn backend_from_env() -> Option<S3StorageBackend> {
        let bucket = std::env::var("RECURSIVE_TEST_S3_BUCKET").ok()?;
        let prefix =
            std::env::var("RECURSIVE_TEST_S3_PREFIX").unwrap_or_else(|_| "recursive-test".into());
        let tenant = format!("test-{}", uuid::Uuid::new_v4());
        S3StorageBackend::new(bucket, prefix, tenant).await.ok()
    }

    #[tokio::test]
    async fn s3_transcript_roundtrip() {
        let Some(backend) = backend_from_env().await else {
            return; // skip when no S3 credentials available
        };
        use crate::message::Role;
        let msgs = vec![Message {
            role: Role::User,
            content: "hello s3".into(),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: None,
        }];
        let session_id = "test-session";
        timeout(
            Duration::from_secs(10),
            backend.save_transcript(session_id, &msgs),
        )
        .await
        .expect("timeout")
        .expect("save");
        let loaded = timeout(Duration::from_secs(10), backend.load_transcript(session_id))
            .await
            .expect("timeout")
            .expect("load");
        assert_eq!(loaded, msgs);
    }

    #[tokio::test]
    async fn s3_transcript_nonexistent_returns_empty() {
        let Some(backend) = backend_from_env().await else {
            return;
        };
        let loaded = timeout(
            Duration::from_secs(10),
            backend.load_transcript("no-such-session"),
        )
        .await
        .expect("timeout")
        .expect("load");
        assert!(loaded.is_empty());
    }

    #[tokio::test]
    async fn s3_memory_roundtrip() {
        let Some(backend) = backend_from_env().await else {
            return;
        };
        timeout(
            Duration::from_secs(10),
            backend.save_memory("summary.md", "content"),
        )
        .await
        .expect("timeout")
        .expect("save");
        let val = timeout(Duration::from_secs(10), backend.load_memory("summary.md"))
            .await
            .expect("timeout")
            .expect("load");
        assert_eq!(val.as_deref(), Some("content"));
    }

    #[tokio::test]
    async fn s3_memory_missing_returns_none() {
        let Some(backend) = backend_from_env().await else {
            return;
        };
        let val = timeout(Duration::from_secs(10), backend.load_memory("no-such-key"))
            .await
            .expect("timeout")
            .expect("load");
        assert!(val.is_none());
    }
}
