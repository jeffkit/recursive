//! Shared fixtures for the HTTP API integration tests.
//!
//! Extracted from the monolithic `tests/http.rs` during the P0-3 cleanup
//! so that fixture builders (mock config / app state) live next to their
//! consumers but are not intermingled with `#[test]` bodies. This makes
//! the file easier to read and primes the ground for a future per-feature-
//! area split of `tests/http.rs` — every consumer file will then
//! `#[allow(dead_code)] mod common;` to share the same fixtures.
//!
//! Note: this module sits under `tests/http_common/mod.rs` rather than
//! `tests/http_common.rs` so cargo does NOT treat it as an additional
//! integration test target. (Every `tests/*.rs` is its own test binary;
//! a top-level `http_common.rs` would compile to a "no tests" binary
//! and produce cargo warnings.) See
//! <https://doc.rust-lang.org/book/ch11-03-test-organization.html#submodules-in-integration-tests>
//! for the canonical pattern.

#![allow(dead_code)]

use recursive::config::Config;
use recursive::http::{AppState, Metrics, RateLimiter, ToolInfo};
use recursive::llm::{Completion, MockProvider};
use recursive::message::Message;
use recursive::storage::StorageBackend;
use recursive::tools::ToolRegistry;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Since Goal 277, the HTTP server default-deny requires the
/// insecure-ok debug escape hatch for no-auth integration tests.
pub static SET_INSECURE_OK: std::sync::Once = std::sync::Once::new();

// ── Goal 396: in-memory StorageBackend for HTTP fixtures ─────────────────

/// One `save_transcript` call recorded by [`MemoryStorage`].
#[derive(Clone, Debug)]
pub struct SaveRecord {
    pub session_id: String,
    pub messages: Vec<Message>,
    /// When a probe lock is attached: `true` if the probed lock was FREE at
    /// save time (i.e. the host did NOT hold it across the save).
    pub probe_lock_was_free: Option<bool>,
}

/// Goal 396: in-memory fake `StorageBackend`.
///
/// Records every save so tests can assert what the host layer persisted,
/// and optionally probes the host's **sessions map lock** during the save
/// to pin the lock-scope rule ("save_transcript must run outside the host
/// sessions lock": if the host held the write lock across the save,
/// `try_write` fails and the test fails). Round-trip via `load_transcript`
/// also works.
#[derive(Default)]
pub struct MemoryStorage {
    pub saves: std::sync::Mutex<Vec<SaveRecord>>,
    /// Optional probe — the host sessions map itself, probed for write
    /// access at save time.
    pub probe_sessions: Option<Arc<RwLock<HashMap<String, recursive::http::SessionState>>>>,
    /// Backing store for round-trip reads (session_id → jsonl lines).
    store: std::sync::Mutex<HashMap<String, Vec<String>>>,
    /// Named memory entries (per-session tombstones, shared memory, …).
    memory: std::sync::Mutex<HashMap<String, String>>,
    /// Sessions passed to `delete_transcript`, in call order.
    pub deleted: std::sync::Mutex<Vec<String>>,
    /// Requested retention windows passed to `purge_expired_sessions`.
    pub purges: std::sync::Mutex<Vec<std::time::Duration>>,
    /// Live-session ids passed alongside each retention window.
    pub purge_keep: std::sync::Mutex<Vec<Vec<String>>>,
    /// Value `purge_expired_sessions` reports (the number "removed").
    pub purge_result: std::sync::atomic::AtomicUsize,
    /// Issue #92: per-turn appends, kept separate from `saves` so tests that
    /// assert on full-save counts (DELETE / eviction / shutdown) are not
    /// perturbed by the runtime's incremental writes.
    pub appends: std::sync::Mutex<Vec<SaveRecord>>,
}

impl MemoryStorage {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn with_sessions_probe(
        sessions: Arc<RwLock<HashMap<String, recursive::http::SessionState>>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            probe_sessions: Some(sessions),
            ..Default::default()
        })
    }

    pub fn saves(&self) -> Vec<SaveRecord> {
        self.saves.lock().unwrap().clone()
    }

    pub fn deleted(&self) -> Vec<String> {
        self.deleted.lock().unwrap().clone()
    }

    pub fn purges(&self) -> Vec<std::time::Duration> {
        self.purges.lock().unwrap().clone()
    }

    pub fn has_memory(&self, key: &str) -> bool {
        self.memory.lock().unwrap().contains_key(key)
    }

    pub fn appends(&self) -> Vec<SaveRecord> {
        self.appends.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl StorageBackend for MemoryStorage {
    async fn load_transcript(&self, session_id: &str) -> recursive::error::Result<Vec<Message>> {
        let store = self.store.lock().unwrap();
        let lines = store.get(session_id).cloned().unwrap_or_default();
        drop(store);
        lines
            .iter()
            .map(|l| serde_json::from_str(l))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| recursive::error::Error::Config {
                message: format!("memory storage parse: {e}"),
            })
    }

    async fn save_transcript(
        &self,
        session_id: &str,
        messages: &[Message],
    ) -> recursive::error::Result<()> {
        // Probe BEFORE any await point: if the host still held the sessions
        // write lock here, try_write fails and the test fails.
        let probe_lock_was_free = self
            .probe_sessions
            .as_ref()
            .map(|sessions| sessions.try_write().is_ok());
        let lines: Vec<String> = messages
            .iter()
            .map(|m| {
                serde_json::to_string(m).map_err(|e| recursive::error::Error::Config {
                    message: format!("memory storage serialize: {e}"),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.store
            .lock()
            .unwrap()
            .insert(session_id.to_string(), lines);
        self.saves.lock().unwrap().push(SaveRecord {
            session_id: session_id.to_string(),
            messages: messages.to_vec(),
            probe_lock_was_free,
        });
        Ok(())
    }

    /// Issue #92: the runtime's per-turn path. Simulates a native append
    /// (extend the stored lines) and records into `appends`, not `saves`.
    async fn append_transcript(
        &self,
        session_id: &str,
        messages: &[Message],
    ) -> recursive::error::Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let new_lines: Vec<String> = messages
            .iter()
            .map(|m| {
                serde_json::to_string(m).map_err(|e| recursive::error::Error::Config {
                    message: format!("memory storage serialize: {e}"),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.store
            .lock()
            .unwrap()
            .entry(session_id.to_string())
            .or_default()
            .extend(new_lines);
        self.appends.lock().unwrap().push(SaveRecord {
            session_id: session_id.to_string(),
            messages: messages.to_vec(),
            probe_lock_was_free: None,
        });
        Ok(())
    }

    async fn delete_transcript(&self, session_id: &str) -> recursive::error::Result<()> {
        self.store.lock().unwrap().remove(session_id);
        self.deleted.lock().unwrap().push(session_id.to_string());
        Ok(())
    }

    async fn load_memory(&self, key: &str) -> recursive::error::Result<Option<String>> {
        Ok(self.memory.lock().unwrap().get(key).cloned())
    }

    async fn save_memory(&self, key: &str, value: &str) -> recursive::error::Result<()> {
        self.memory
            .lock()
            .unwrap()
            .insert(key.to_string(), value.to_string());
        Ok(())
    }

    async fn delete_memory(&self, key: &str) -> recursive::error::Result<()> {
        self.memory.lock().unwrap().remove(key);
        Ok(())
    }

    async fn purge_expired_sessions(
        &self,
        max_age: std::time::Duration,
        keep: &std::collections::HashSet<String>,
    ) -> recursive::error::Result<usize> {
        self.purges.lock().unwrap().push(max_age);
        let mut keep: Vec<String> = keep.iter().cloned().collect();
        keep.sort();
        self.purge_keep.lock().unwrap().push(keep);
        Ok(self.purge_result.load(std::sync::atomic::Ordering::Relaxed))
    }
}

/// Storage for fixtures that don't assert on persistence — keeps test
/// transcripts out of the real filesystem entirely.
pub fn memory_storage() -> Arc<MemoryStorage> {
    MemoryStorage::new()
}

pub fn mock_config() -> Config {
    Config {
        workspace: PathBuf::from("/tmp"),
        api_base: "https://example.invalid/v1".into(),
        api_key: Some("test-key".into()),
        model: "mock".into(),
        provider_type: "openai".into(),
        preset: None,
        max_steps: 32,
        max_tokens: 65536,
        temperature: 0.0,
        system_prompt: "You are a test assistant.".into(),
        retry_max: 0,
        retry_initial_backoff_secs: 1,
        retry_max_backoff_secs: 1,
        shell_timeout_secs: 5,
        headless: false,
        memory_summary_limit: 5,
        thinking_budget: None,
        session_name: None,
        max_budget_usd: None,
        extra_dirs: Vec::new(),
        extra_readonly_dirs: Vec::new(),
        allow_tools: Vec::new(),
        context_window_override: None,
        subagent_max_depth: 2,
        subagent_enabled: false,
        allow_bypass_permissions: false,
        max_search_rounds: 3,
        stuck_window: 10,
        stuck_error_rate: 0.8,
        max_concurrent_runs: 8,
        goal_eval_transcript_tail: 12,
        web_search_provider: None,
        web_search_api_key: None,
        web_search_jina_key: None,
        wall_timeout_secs: 0,
    }
}

pub fn sample_state() -> AppState {
    SET_INSECURE_OK.call_once(|| {
        unsafe { std::env::set_var("RECURSIVE_HTTP_AUTH_INSECURE_OK", "1") };
    });
    let provider = Arc::new(MockProvider::new(vec![Completion {
        content: "hello".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));
    AppState {
        tools: vec![
            ToolInfo {
                name: "Read".into(),
                description: "Read a file from the workspace".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" }
                    },
                    "required": ["path"]
                }),
            },
            ToolInfo {
                name: "Write".into(),
                description: "Write content to a file".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" }
                    },
                    "required": ["path", "content"]
                }),
            },
        ],
        config: mock_config(),
        tool_registry: ToolRegistry::local(),
        provider,
        event_channels: Arc::new(RwLock::new(HashMap::new())),
        metrics: Arc::new(Metrics::default()),
        slash_commands: Arc::new(Vec::new()),
        host: std::sync::Arc::new(recursive::session_host::SessionHost::new(
            std::time::Duration::from_secs(0),
            recursive::http::AdmissionGate::new(
                8,
                std::time::Duration::ZERO,
                std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            ),
        )),
        rate_limiter: RateLimiter::new(10, 1.0),
        skills: vec![],
        storage: test_storage_dir(),
        agui_active_runs: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        // Issue #121: no native session mirror for the shared fixtures.
        session_mirror_root: None,
    }
}

pub fn sample_state_with_provider(provider: Arc<MockProvider>) -> AppState {
    SET_INSECURE_OK.call_once(|| {
        unsafe { std::env::set_var("RECURSIVE_HTTP_AUTH_INSECURE_OK", "1") };
    });
    AppState {
        tools: vec![],
        config: mock_config(),
        tool_registry: ToolRegistry::local(),
        provider,
        event_channels: Arc::new(RwLock::new(HashMap::new())),
        metrics: Arc::new(Metrics::default()),
        slash_commands: Arc::new(Vec::new()),
        host: std::sync::Arc::new(recursive::session_host::SessionHost::new(
            std::time::Duration::from_secs(0),
            recursive::http::AdmissionGate::new(
                8,
                std::time::Duration::ZERO,
                std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            ),
        )),
        rate_limiter: RateLimiter::new(10, 1.0),
        skills: vec![],
        storage: test_storage_dir(),
        agui_active_runs: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        // Issue #121: no native session mirror for the shared fixtures.
        session_mirror_root: None,
    }
}

/// Throwaway per-process storage root for fixtures that don't care about
/// persistence — `load_transcript` on the missing dir yields empty `Vec`s,
/// so cold load keeps every unknown session a 404.
fn test_storage_dir() -> Arc<dyn recursive::storage::StorageBackend> {
    Arc::new(recursive::storage::LocalStorageBackend::new(
        std::env::temp_dir().join(format!("recursive-http-test-{}", std::process::id())),
    ))
}

/// Goal 397: fixture with an explicit storage backend so cold-load tests can
/// seed transcripts the way a previous process would have written them.
pub fn sample_state_with_storage(
    provider: Arc<MockProvider>,
    storage: Arc<dyn recursive::storage::StorageBackend>,
) -> AppState {
    SET_INSECURE_OK.call_once(|| {
        unsafe { std::env::set_var("RECURSIVE_HTTP_AUTH_INSECURE_OK", "1") };
    });
    AppState {
        tools: vec![],
        config: mock_config(),
        tool_registry: ToolRegistry::local(),
        provider,
        host: std::sync::Arc::new(recursive::session_host::SessionHost::new(
            std::time::Duration::from_secs(0),
            recursive::http::AdmissionGate::new(
                8,
                std::time::Duration::ZERO,
                std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            ),
        )),
        event_channels: Arc::new(RwLock::new(HashMap::new())),
        metrics: Arc::new(Metrics::default()),
        slash_commands: Arc::new(Vec::new()),
        rate_limiter: RateLimiter::new(10, 1.0),
        skills: vec![],
        storage,
        agui_active_runs: Arc::new(std::sync::Mutex::new(HashMap::new())),
        // Issue #121: no native session mirror for the shared fixtures.
        session_mirror_root: None,
    }
}

// ── Issue #121: the native session mirror ────────────────────────────────
//
// Every fixture below disables the mirror (`session_mirror_root: None`).
// `flush_all_sessions` / idle eviction would otherwise write into whatever
// `paths::user_sessions_dir` resolves to — the developer's real store — and
// pointing that at a tempdir means overriding `RECURSIVE_HOME` /
// `RECURSIVE_SESSIONS_DIR`, which are process-global and would redirect every
// *other* concurrently-running test in this binary that resolves
// env-derived state (`TriggerStore::default_path` → `user_workspace_dir`).
// The mirror root is injected per `AppState` precisely so no test has to.
