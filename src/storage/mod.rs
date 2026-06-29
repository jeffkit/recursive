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

#[cfg(feature = "workspace-store")]
pub mod workspace_store;
#[cfg(feature = "workspace-store")]
pub use workspace_store::{PathEntry, SqliteWorkspaceStore, WorkspaceStore};

#[cfg(all(target_os = "linux", feature = "workspace-fuse"))]
pub mod workspace_fuse;
#[cfg(all(target_os = "linux", feature = "workspace-fuse"))]
pub use workspace_fuse::{WorkspaceFuse, WorkspaceFuseHandle};

#[cfg(feature = "cloud-runtime")]
pub mod redis;
#[cfg(feature = "cloud-runtime")]
pub use redis::RedisSessionStore;

#[cfg(feature = "cloud-runtime")]
pub mod s3;
#[cfg(feature = "cloud-runtime")]
pub use s3::S3StorageBackend;

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
}
