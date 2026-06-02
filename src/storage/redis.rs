//! Redis-backed [`SessionStore`] for cloud multi-tenant deployments.
//!
//! Stores [`AgentCheckpointState`] as JSON in Redis with a configurable TTL
//! so pods can recover agent hot-state after a restart without losing the
//! current loop position.
//!
//! Gated behind the `cloud-runtime` feature flag.

use std::time::Duration;

use deadpool_redis::{Config, Pool, Runtime};
use redis::AsyncCommands;

use crate::error::{Error, Result};
use crate::storage::{AgentCheckpointState, SessionStore};

/// Redis-backed implementation of [`SessionStore`].
///
/// Each session is stored under `<key_prefix>:session:<session_id>` and
/// expires after `ttl` of inactivity (renewed on every `save_state` call).
pub struct RedisSessionStore {
    pool: Pool,
    /// Key TTL — sessions auto-expire after this duration of inactivity.
    ttl: Duration,
    key_prefix: String,
}

impl RedisSessionStore {
    /// Connect to `redis_url` and create a pooled store.
    ///
    /// `ttl` is applied on every `save_state` call (default: 2 hours).
    /// `key_prefix` namespaces keys to prevent collisions between tenants.
    pub fn new(redis_url: &str, ttl: Duration, key_prefix: impl Into<String>) -> Result<Self> {
        let cfg = Config::from_url(redis_url);
        let pool = cfg
            .create_pool(Some(Runtime::Tokio1))
            .map_err(|e| Error::Storage {
                message: format!("create redis pool: {e}"),
            })?;
        Ok(Self {
            pool,
            ttl,
            key_prefix: key_prefix.into(),
        })
    }

    /// Create with the default 2-hour TTL.
    pub fn with_default_ttl(redis_url: &str, key_prefix: impl Into<String>) -> Result<Self> {
        Self::new(redis_url, Duration::from_secs(7200), key_prefix)
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
            message: format!("serialize checkpoint state: {e}"),
        })?;
        let ttl_secs = self.ttl.as_secs();
        conn.set_ex::<_, _, ()>(&key, value, ttl_secs)
            .await
            .map_err(|e| Error::Storage {
                message: format!("redis SET EX {key}: {e}"),
            })
    }

    async fn load_state(&self, session_id: &str) -> Result<Option<AgentCheckpointState>> {
        let mut conn = self.pool.get().await.map_err(|e| Error::Storage {
            message: format!("redis pool get: {e}"),
        })?;
        let key = self.key(session_id);
        let raw: Option<String> = conn.get(&key).await.map_err(|e| Error::Storage {
            message: format!("redis GET {key}: {e}"),
        })?;
        match raw {
            None => Ok(None),
            Some(v) => {
                let state = serde_json::from_str::<AgentCheckpointState>(&v).map_err(|e| {
                    Error::Storage {
                        message: format!("deserialize checkpoint state for {session_id}: {e}"),
                    }
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
            message: format!("redis DEL {key}: {e}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::timeout;

    #[tokio::test]
    async fn redis_session_store_roundtrip() {
        let url = match std::env::var("RECURSIVE_TEST_REDIS_URL") {
            Ok(u) => u,
            Err(_) => return, // skip when no Redis is available
        };
        let store =
            RedisSessionStore::new(&url, Duration::from_secs(60), "test").expect("create store");
        let session_id = format!("test-{}", uuid::Uuid::new_v4());
        let state = AgentCheckpointState {
            step: 7,
            transcript_len: 42,
        };

        timeout(
            Duration::from_secs(5),
            store.save_state(&session_id, &state),
        )
        .await
        .expect("timeout")
        .expect("save_state");

        let loaded = timeout(Duration::from_secs(5), store.load_state(&session_id))
            .await
            .expect("timeout")
            .expect("load_state");
        assert_eq!(loaded, Some(state));

        timeout(Duration::from_secs(5), store.delete_state(&session_id))
            .await
            .expect("timeout")
            .expect("delete_state");

        let after_delete = timeout(Duration::from_secs(5), store.load_state(&session_id))
            .await
            .expect("timeout")
            .expect("load after delete");
        assert_eq!(after_delete, None);
    }

    #[tokio::test]
    async fn redis_session_store_load_missing_returns_none() {
        let url = match std::env::var("RECURSIVE_TEST_REDIS_URL") {
            Ok(u) => u,
            Err(_) => return,
        };
        let store = RedisSessionStore::new(&url, Duration::from_secs(60), "test-missing")
            .expect("create store");
        let result = timeout(Duration::from_secs(5), store.load_state("no-such-session"))
            .await
            .expect("timeout")
            .expect("load_state");
        assert_eq!(result, None);
    }
}
