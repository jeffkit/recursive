//! Storage abstraction layer for Recursive.
//!
//! Defines the traits that decouple the agent kernel from specific
//! storage backends (local filesystem, Redis, S3, etc.).
//!
//! # Design
//!
//! Two orthogonal traits cover all persistent data needs:
//!
//! - [`StorageBackend`]: long-lived data — transcript and memory entries.
//!   Implementations include [`local::LocalStorageBackend`] (files) and,
//!   in cloud deployments, S3 or Postgres.
//!
//! - [`SessionStore`]: short-lived hot state used for crash recovery.
//!   The local implementation [`NoopSessionStore`] is a zero-cost no-op.
//!   Cloud implementations write to Redis with a TTL.
//!
//! The kernel accepts these traits via dependency injection, so the same
//! code runs in both local and multi-tenant cloud modes.

pub mod local;
pub use local::LocalStorageBackend;

#[cfg(feature = "cloud-runtime")]
pub mod redis;
#[cfg(feature = "cloud-runtime")]
pub use redis::RedisSessionStore;

#[cfg(feature = "cloud-runtime")]
pub mod s3;
#[cfg(feature = "cloud-runtime")]
pub use s3::S3StorageBackend;

use std::path::PathBuf;
use std::sync::Arc;

use crate::error::Result;
use crate::message::Message;
use async_trait::async_trait;

// ─────────────────────────────────────────────────────────────────────────────
// StorageBackend
// ─────────────────────────────────────────────────────────────────────────────

/// Persistent storage for session transcript and memory entries.
///
/// # Semantics
///
/// - `load_transcript` returns an empty `Vec` (not an error) when the session
///   does not yet exist.
/// - `load_memory` returns `None` (not an error) when the key does not exist.
/// - Implementations must be safe to call concurrently from multiple async
///   tasks (`Send + Sync + 'static`).
///
/// The trait uses `#[async_trait]` so it is `dyn`-compatible and can be
/// stored as `Arc<dyn StorageBackend>` without generics spreading to callers.
#[async_trait]
pub trait StorageBackend: Send + Sync + 'static {
    /// Load the full transcript for a session.
    ///
    /// Returns `Ok(vec![])` if the session has no persisted transcript yet.
    async fn load_transcript(&self, session_id: &str) -> Result<Vec<Message>>;

    /// Persist the full transcript for a session.
    ///
    /// This is a full overwrite — the caller is responsible for appending
    /// new messages before calling this.
    async fn save_transcript(&self, session_id: &str, messages: &[Message]) -> Result<()>;

    /// Append messages to a session's persisted transcript.
    ///
    /// This is the incremental-persistence path (issue #92): the HTTP host
    /// calls it after every turn so a crashed or OOM-killed pod loses at most
    /// the in-flight turn, instead of every turn since the last teardown.
    ///
    /// `messages` are the *new* messages only — callers track what has already
    /// been persisted, so a full transcript must never be passed here (that
    /// would duplicate the prefix). Appending an empty slice is a no-op.
    ///
    /// The default implementation is a load-extend-save fallback: correct for
    /// any backend, but it rewrites the whole record, so a backend that can
    /// append natively (e.g. [`LocalStorageBackend`]) overrides it.
    async fn append_transcript(&self, session_id: &str, messages: &[Message]) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let mut existing = self.load_transcript(session_id).await?;
        existing.extend_from_slice(messages);
        self.save_transcript(session_id, &existing).await
    }

    /// Load a named memory entry (e.g. `"user.md"`, `"project.md"`).
    ///
    /// Returns `Ok(None)` if the key has never been written.
    async fn load_memory(&self, key: &str) -> Result<Option<String>>;

    /// Store a named memory entry.
    async fn save_memory(&self, key: &str, value: &str) -> Result<()>;
}

// ─────────────────────────────────────────────────────────────────────────────
// AgentCheckpointState
// ─────────────────────────────────────────────────────────────────────────────

/// Opaque snapshot of in-flight agent state for crash recovery / pod migration.
///
/// Kept intentionally minimal: only what is needed to resume the Agent Loop
/// after a pod restart or failover. Full transcript reconstruction uses
/// `StorageBackend::load_transcript`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct AgentCheckpointState {
    /// Current step index inside the Agent Loop (0-based).
    pub step: usize,
    /// Number of messages in the transcript at the time of this checkpoint.
    /// Used to verify the transcript is consistent before resuming.
    pub transcript_len: usize,
}

// ─────────────────────────────────────────────────────────────────────────────
// SessionStore
// ─────────────────────────────────────────────────────────────────────────────

/// Hot-state store for in-flight Agent Loop checkpoints.
///
/// The local default implementation is [`NoopSessionStore`] — a zero-cost
/// no-op that never persists anything. Cloud implementations write to Redis
/// with a short TTL to enable crash recovery across pod restarts.
///
/// # Semantics
///
/// - Checkpoint failures are non-fatal by design: the Agent Loop MUST NOT
///   abort because a `save_state` call failed.  Callers should log the error
///   and continue.
/// - `load_state` returns `Ok(None)` when no checkpoint exists (new session).
/// - `delete_state` is idempotent: deleting a non-existent key is `Ok(())`.
///
/// Uses `#[async_trait]` so it is `dyn`-compatible (`Arc<dyn SessionStore>`).
#[async_trait]
pub trait SessionStore: Send + Sync + 'static {
    /// Persist the current loop state for a session.
    ///
    /// Called after each tool execution.  Failures should be treated as
    /// warnings, not errors.
    async fn save_state(&self, session_id: &str, state: &AgentCheckpointState) -> Result<()>;

    /// Load the most recent checkpoint for a session.
    ///
    /// Returns `Ok(None)` if no checkpoint exists (fresh session or already
    /// cleaned up).
    async fn load_state(&self, session_id: &str) -> Result<Option<AgentCheckpointState>>;

    /// Remove all checkpoint state for a session.
    ///
    /// Should be called after the Agent Loop finishes (success or failure) to
    /// avoid stale state in Redis/KV.
    async fn delete_state(&self, session_id: &str) -> Result<()>;
}

// ─────────────────────────────────────────────────────────────────────────────
// NoopSessionStore
// ─────────────────────────────────────────────────────────────────────────────

/// In-memory no-op `SessionStore`.
///
/// Used in local single-machine mode where crash recovery is not needed.
/// All operations are immediate `Ok(())` / `Ok(None)` with zero overhead.
pub struct NoopSessionStore;

#[async_trait]
impl SessionStore for NoopSessionStore {
    async fn save_state(&self, _session_id: &str, _state: &AgentCheckpointState) -> Result<()> {
        Ok(())
    }

    async fn load_state(&self, _session_id: &str) -> Result<Option<AgentCheckpointState>> {
        Ok(None)
    }

    async fn delete_state(&self, _session_id: &str) -> Result<()> {
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// HTTP backend selection (issue #92)
// ─────────────────────────────────────────────────────────────────────────────

/// Env var selecting the S3 transcript backend (`cloud-runtime`).
pub const ENV_S3_BUCKET: &str = "RECURSIVE_S3_BUCKET";
/// Env var overriding the S3 object-key prefix.
pub const ENV_S3_PREFIX: &str = "RECURSIVE_S3_PREFIX";
/// Env var overriding the S3 tenant namespace inside the bucket.
pub const ENV_S3_TENANT_ID: &str = "RECURSIVE_S3_TENANT_ID";
/// Env var selecting a Redis session store.
///
/// Recognised but not consumed by `recursive http` yet — see
/// [`warn_unwired_cloud_env`].
pub const ENV_REDIS_URL: &str = "RECURSIVE_REDIS_URL";

/// Default S3 key prefix when [`ENV_S3_PREFIX`] is unset.
pub const DEFAULT_S3_PREFIX: &str = "recursive";
/// Default S3 tenant namespace when [`ENV_S3_TENANT_ID`] is unset.
pub const DEFAULT_S3_TENANT_ID: &str = "default";

/// The transcript/memory backend `recursive http` should open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpStorage {
    /// Local filesystem backend under the per-workspace user dir.
    Local,
    /// S3 backend — shared across replicas, so cold load works after a
    /// restart or on a different pod.
    S3 {
        bucket: String,
        prefix: String,
        tenant_id: String,
    },
}

/// Decide the HTTP transcript backend from an environment lookup.
///
/// Pure over `lookup`, so the decision is testable without process-global env
/// vars, the `cloud-runtime` feature, or a live S3 endpoint. A non-blank
/// [`ENV_S3_BUCKET`] selects S3; [`ENV_S3_PREFIX`] / [`ENV_S3_TENANT_ID`]
/// fall back to [`DEFAULT_S3_PREFIX`] / [`DEFAULT_S3_TENANT_ID`].
pub fn select_http_storage(lookup: &dyn Fn(&str) -> Option<String>) -> HttpStorage {
    match non_blank(lookup, ENV_S3_BUCKET) {
        None => HttpStorage::Local,
        Some(bucket) => HttpStorage::S3 {
            bucket,
            prefix: non_blank(lookup, ENV_S3_PREFIX)
                .unwrap_or_else(|| DEFAULT_S3_PREFIX.to_string()),
            tenant_id: non_blank(lookup, ENV_S3_TENANT_ID)
                .unwrap_or_else(|| DEFAULT_S3_TENANT_ID.to_string()),
        },
    }
}

fn non_blank(lookup: &dyn Fn(&str) -> Option<String>, key: &str) -> Option<String> {
    lookup(key).filter(|value| !value.trim().is_empty())
}

/// Open the transcript/memory backend `recursive http` should use, reading the
/// process environment (issue #92).
///
/// [`ENV_S3_BUCKET`] selects S3 when the `cloud-runtime` feature is compiled
/// in; otherwise the local filesystem backend rooted at `workspace_root` is
/// returned. Cloud env vars the HTTP server does not consume are logged by
/// [`warn_unwired_cloud_env`].
///
/// Not mutated: this is environment plumbing whose decision is pinned by
/// [`select_http_storage`]'s tests, and whose S3 arm needs a live endpoint.
#[cfg(feature = "cloud-runtime")]
#[cfg_attr(test, mutants::skip)]
pub async fn http_storage_backend(workspace_root: PathBuf) -> Result<Arc<dyn StorageBackend>> {
    warn_unwired_cloud_env();
    match select_http_storage(&|key| std::env::var(key).ok()) {
        HttpStorage::Local => Ok(Arc::new(LocalStorageBackend::new(workspace_root))),
        HttpStorage::S3 {
            bucket,
            prefix,
            tenant_id,
        } => {
            tracing::info!(%bucket, %prefix, %tenant_id, "http storage: S3StorageBackend");
            Ok(Arc::new(
                S3StorageBackend::new(bucket, prefix, tenant_id).await?,
            ))
        }
    }
}

/// [`http_storage_backend`] without the `cloud-runtime` feature: always local.
#[cfg(not(feature = "cloud-runtime"))]
#[cfg_attr(test, mutants::skip)]
pub async fn http_storage_backend(workspace_root: PathBuf) -> Result<Arc<dyn StorageBackend>> {
    warn_unwired_cloud_env();
    Ok(Arc::new(LocalStorageBackend::new(workspace_root)))
}

/// Log cloud env knobs the HTTP server does not consume yet (issue #92).
///
/// - Redis hot-state: [`ENV_REDIS_URL`] is recognised but no
///   `RedisSessionStore` is built — the kernel owns the `SessionStore`
///   injection point but never checkpoints per turn, so a store would be a
///   no-op today. The type stays reachable through
///   `AgentRuntimeBuilder::session_store`.
/// - S3 without the feature: [`ENV_S3_BUCKET`] only takes effect when the
///   `cloud-runtime` feature is compiled in.
///
/// Both checks treat a blank value as unset (via [`non_blank`]), matching
/// [`select_http_storage`] — `.env.example` ships empty defaults, so a blank
/// var must not produce a spurious warning. `warn!` so the notice survives a
/// `--log warn` / `RUST_LOG=warn` filter.
///
/// Logging only, no observable behaviour to pin.
#[cfg_attr(test, mutants::skip)]
pub fn warn_unwired_cloud_env() {
    let env = |key: &str| std::env::var(key).ok();
    if non_blank(&env, ENV_REDIS_URL).is_some() {
        tracing::warn!(
            "RECURSIVE_REDIS_URL is set but `recursive http` does not build a \
             RedisSessionStore: the kernel has no per-turn checkpoint consumer, so it \
             would be a no-op. Redis stays available through the library API."
        );
    }
    if !cfg!(feature = "cloud-runtime") && non_blank(&env, ENV_S3_BUCKET).is_some() {
        tracing::warn!(
            "RECURSIVE_S3_BUCKET is set but this build was compiled without the \
             `cloud-runtime` feature; using LocalStorageBackend."
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_session_store_is_always_empty() {
        let store = NoopSessionStore;
        let state = store.load_state("test-session").await.unwrap();
        assert!(state.is_none());
    }

    #[tokio::test]
    async fn noop_session_store_save_and_delete_are_noop() {
        let store = NoopSessionStore;
        let checkpoint = AgentCheckpointState {
            step: 3,
            transcript_len: 7,
        };

        store.save_state("session-1", &checkpoint).await.unwrap();
        store.delete_state("session-1").await.unwrap();
        // After save + delete, still returns None (noop never stores anything)
        let loaded = store.load_state("session-1").await.unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn agent_checkpoint_state_serializes() {
        let state = AgentCheckpointState {
            step: 5,
            transcript_len: 12,
        };
        let json = serde_json::to_string(&state).unwrap();
        let roundtripped: AgentCheckpointState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, roundtripped);
    }

    #[test]
    fn agent_checkpoint_state_zero_is_valid() {
        let state = AgentCheckpointState {
            step: 0,
            transcript_len: 0,
        };
        let json = serde_json::to_string(&state).unwrap();
        let rt: AgentCheckpointState = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.step, 0);
        assert_eq!(rt.transcript_len, 0);
    }

    // ── issue #92: default `append_transcript` (load-extend-save) ─────────

    /// Minimal backend that does NOT override `append_transcript`, so the
    /// trait default (the S3 / non-native-append path) is exercised.
    #[derive(Default)]
    struct LoadExtendSaveStorage {
        transcript: std::sync::Mutex<Vec<Message>>,
    }

    #[async_trait::async_trait]
    impl StorageBackend for LoadExtendSaveStorage {
        async fn load_transcript(&self, _session_id: &str) -> Result<Vec<Message>> {
            Ok(self.transcript.lock().unwrap().clone())
        }

        async fn save_transcript(&self, _session_id: &str, messages: &[Message]) -> Result<()> {
            *self.transcript.lock().unwrap() = messages.to_vec();
            Ok(())
        }

        async fn load_memory(&self, _key: &str) -> Result<Option<String>> {
            Ok(None)
        }

        async fn save_memory(&self, _key: &str, _value: &str) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn default_append_transcript_extends_existing_transcript() {
        let backend = LoadExtendSaveStorage::default();
        backend
            .save_transcript("s", &[Message::user("a")])
            .await
            .unwrap();
        backend
            .append_transcript("s", &[Message::assistant("b")])
            .await
            .unwrap();

        let stored = backend.load_transcript("s").await.unwrap();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].content, "a");
        assert_eq!(stored[1].content, "b");
    }

    #[tokio::test]
    async fn default_append_transcript_empty_slice_is_noop() {
        let backend = LoadExtendSaveStorage::default();
        backend.append_transcript("s", &[]).await.unwrap();
        assert!(backend.load_transcript("s").await.unwrap().is_empty());
    }

    // ── issue #92: HTTP backend selection ────────────────────────────────

    #[test]
    fn select_http_storage_defaults_to_local_when_unset() {
        let env = |_: &str| None;
        assert_eq!(select_http_storage(&env), HttpStorage::Local);
    }

    #[test]
    fn select_http_storage_reads_bucket_and_fills_defaults() {
        let env = |key: &str| (key == ENV_S3_BUCKET).then(|| "my-bucket".to_string());
        assert_eq!(
            select_http_storage(&env),
            HttpStorage::S3 {
                bucket: "my-bucket".to_string(),
                prefix: DEFAULT_S3_PREFIX.to_string(),
                tenant_id: DEFAULT_S3_TENANT_ID.to_string(),
            }
        );
    }

    #[test]
    fn select_http_storage_honours_prefix_and_tenant_overrides() {
        let env = |key: &str| match key {
            ENV_S3_BUCKET => Some("b".to_string()),
            ENV_S3_PREFIX => Some("transcripts".to_string()),
            ENV_S3_TENANT_ID => Some("acme".to_string()),
            _ => None,
        };
        assert_eq!(
            select_http_storage(&env),
            HttpStorage::S3 {
                bucket: "b".to_string(),
                prefix: "transcripts".to_string(),
                tenant_id: "acme".to_string(),
            }
        );
    }

    #[test]
    fn select_http_storage_ignores_blank_bucket() {
        let env = |key: &str| (key == ENV_S3_BUCKET).then(|| "   ".to_string());
        assert_eq!(select_http_storage(&env), HttpStorage::Local);
    }

    #[test]
    fn select_http_storage_blank_prefix_and_tenant_fall_back_to_defaults() {
        let env = |key: &str| match key {
            ENV_S3_BUCKET => Some("b".to_string()),
            ENV_S3_PREFIX => Some(String::new()),
            ENV_S3_TENANT_ID => Some("  ".to_string()),
            _ => None,
        };
        assert_eq!(
            select_http_storage(&env),
            HttpStorage::S3 {
                bucket: "b".to_string(),
                prefix: DEFAULT_S3_PREFIX.to_string(),
                tenant_id: DEFAULT_S3_TENANT_ID.to_string(),
            }
        );
    }
}
