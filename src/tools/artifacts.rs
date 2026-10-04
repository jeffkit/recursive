//! Artifact handoff protocol for multi-agent collaboration (goal #106).
//!
//! A worker's final report can be far larger than what a tool-result should
//! carry inline: the coordinator transcript trims old tool outputs
//! (`run_core::maybe_trim_transcript`) and compact drops them entirely. An
//! **artifact** is the durable alternative: the full text is persisted to the
//! collaboration [`StorageBackend`] under `multi/artifacts/<id>`, and the
//! worker result carries a small reference block (id, name, byte size, byte
//! budget head/tail) instead of the raw body.
//!
//! Loading is explicit and on-demand via the `artifact_read` tool — the
//! consumer agent decides which artifacts deserve context space.

use crate::error::{Error, Result};
use crate::storage::StorageBackend;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Storage-key prefix under which artifact bodies persist.
/// `LocalStorageBackend` writes them to `<root>/.recursive/memory/<key>`;
/// Redis/S3 backends use `memory/<key>` — same namespace as the bus and
/// shared-memory snapshots.
pub(crate) const ARTIFACT_KEY_PREFIX: &str = "multi/artifacts/";
/// Index key listing artifact metadata (id → name/size/author), so a
/// restarted process can still enumerate what exists.
pub(crate) const ARTIFACT_INDEX_KEY: &str = "multi/artifacts/index.json";

/// A durable, referenceable output produced by a worker (or the coordinator).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactMeta {
    /// Opaque handle: `art-<blake3-16hex>`. This is what tools exchange.
    pub id: String,
    /// Human-readable label (free text, e.g. "benchmark-report").
    pub name: String,
    /// Author (worker id / role name) that produced the artifact.
    pub author: String,
    /// Size of the body in bytes.
    pub size_bytes: usize,
    /// Unix timestamp (seconds) of creation.
    pub created_at: u64,
}

/// How much of an artifact body to embed in the reference block handed back
/// to the caller. Bytes beyond the budget must be fetched via
/// `artifact_read` — that is the entire point of the protocol.
pub(crate) const ARTIFACT_INLINE_BUDGET_BYTES: usize = 2048;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Generate a unique artifact id (same scheme as `multi::generate_message_id`).
fn generate_artifact_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let input = format!("art-{nanos}-{count}");
    let hash = blake3::hash(input.as_bytes());
    format!("art-{}", &hash.to_hex()[..16])
}

/// Render the reference block the producer embeds in its result.
/// Format is stable and asserted by tests; consumers parse by prefix, not
/// by trusting the producer's wording.
pub fn artifact_reference_text(meta: &ArtifactMeta, body: &str) -> String {
    let bytes = body.as_bytes();
    let budget = ARTIFACT_INLINE_BUDGET_BYTES.min(bytes.len());
    let mut out = format!(
        "[artifact saved: id={} name={} bytes={} by {}]\n\
         Use `artifact_read` with id \"{}\" to load the full content.\n",
        meta.id, meta.name, meta.size_bytes, meta.author, meta.id
    );
    if bytes.len() > budget {
        // Head + tail preview so the consumer can triage without a fetch.
        let head = String::from_utf8_lossy(&bytes[..budget]);
        let tail_start = bytes.len().saturating_sub(256);
        let tail = String::from_utf8_lossy(&bytes[tail_start..]);
        out.push_str(&format!("--- preview (first {budget} bytes) ---\n"));
        out.push_str(head.trim_end());
        out.push_str("\n--- preview (last 256 bytes) ---\n");
        out.push_str(tail.trim_end());
        out.push_str("\n--- preview end ---");
    } else {
        out.push_str(body);
    }
    out
}

/// Durable store + in-memory index of collaboration artifacts.
///
/// Bodies live under one storage key each (`multi/artifacts/<id>`) so
/// loading one artifact never deserializes the rest; the small metadata
/// index is a separate key, rewritten on every store.
pub struct ArtifactStore {
    backend: Arc<dyn StorageBackend>,
    index: RwLock<Vec<ArtifactMeta>>,
    restored: AtomicBool,
}

impl ArtifactStore {
    pub fn new(backend: Arc<dyn StorageBackend>) -> Self {
        Self {
            backend,
            index: RwLock::new(Vec::new()),
            restored: AtomicBool::new(false),
        }
    }

    fn body_key(id: &str) -> String {
        format!("{ARTIFACT_KEY_PREFIX}{id}")
    }

    /// Best-effort index restore for a restarted process. A missing or
    /// corrupt index yields an empty in-memory view; bodies remain
    /// loadable by id either way (the index only powers `artifact_list`).
    pub async fn restore(&self) {
        let Ok(Some(json)) = self.backend.load_memory(ARTIFACT_INDEX_KEY).await else {
            return;
        };
        match serde_json::from_str::<Vec<ArtifactMeta>>(&json) {
            Ok(metas) => *self.index.write().await = metas,
            Err(e) => tracing::warn!("artifacts: corrupt index ignored: {e}"),
        }
    }

    /// Rehydrate the index exactly once (best-effort). Safe to call on every
    /// dispatch; the first call restores, later calls are an atomic check.
    pub async fn ensure_restored(&self) {
        if self.restored.swap(true, Ordering::AcqRel) {
            return;
        }
        self.restore().await;
    }

    /// Persist the body, update the index, and return the metadata.
    /// Storage failures are returned (the caller decides whether an
    /// artifact-less fallback is acceptable) — unlike the bus/memory
    /// snapshots, an artifact that failed to save must NOT be advertised
    /// as saved.
    pub async fn put(
        &self,
        body: &str,
        name: impl Into<String>,
        author: impl Into<String>,
    ) -> Result<ArtifactMeta> {
        let meta = ArtifactMeta {
            id: generate_artifact_id(),
            name: name.into(),
            author: author.into(),
            size_bytes: body.len(),
            created_at: now_secs(),
        };
        self.backend
            .save_memory(&Self::body_key(&meta.id), body)
            .await?;
        {
            let mut index = self.index.write().await;
            index.push(meta.clone());
            let json = serde_json::to_string(&*index).unwrap_or_else(|_| "[]".to_string());
            if let Err(e) = self.backend.save_memory(ARTIFACT_INDEX_KEY, &json).await {
                tracing::warn!("artifacts: failed to persist index: {e}");
            }
        }
        Ok(meta)
    }

    /// Load an artifact body by id.
    pub async fn get(&self, id: &str) -> Result<String> {
        if id.contains('/') || id.contains("..") {
            return Err(Error::BadToolArgs {
                name: "artifact_read".into(),
                message: format!("invalid artifact id: '{id}'"),
            });
        }
        match self.backend.load_memory(&Self::body_key(id)).await? {
            Some(body) => Ok(body),
            None => Err(Error::NotFound(format!("artifact '{id}'"))),
        }
    }

    /// Metadata for every artifact this process knows about (live + restored).
    pub async fn list(&self) -> Vec<ArtifactMeta> {
        self.index.read().await.clone()
    }
}

// ---------------------------------------------------------------------------
// artifact_read tool
// ---------------------------------------------------------------------------

/// The `artifact_read` tool — load a persisted artifact body by id.
pub struct ArtifactReadTool {
    store: Arc<ArtifactStore>,
}

impl ArtifactReadTool {
    pub fn new(store: Arc<ArtifactStore>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl crate::tools::Tool for ArtifactReadTool {
    fn spec(&self) -> crate::llm::ToolSpec {
        crate::llm::ToolSpec {
            name: "artifact_read".into(),
            description: "Load the full content of a persisted artifact by id. Worker results \
                          reference large outputs as artifacts (id + preview); use this tool \
                          to fetch a specific artifact into your context on demand."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "The artifact id (e.g. \"art-1a2b3c4d5e6f7a8b\") from a worker result or artifact_list."
                    }
                },
                "required": ["id"]
            }),
        }
    }

    fn side_effect_class(&self) -> crate::tools::ToolSideEffect {
        crate::tools::ToolSideEffect::ReadOnly
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let id = arguments["id"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "artifact_read".into(),
            message: "missing required parameter: id".to_string(),
        })?;
        self.store.get(id).await
    }
}

// ---------------------------------------------------------------------------
// artifact_list tool
// ---------------------------------------------------------------------------

/// The `artifact_list` tool — enumerate known artifacts (id, name, size).
pub struct ArtifactListTool {
    store: Arc<ArtifactStore>,
}

impl ArtifactListTool {
    pub fn new(store: Arc<ArtifactStore>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl crate::tools::Tool for ArtifactListTool {
    fn spec(&self) -> crate::llm::ToolSpec {
        crate::llm::ToolSpec {
            name: "artifact_list".into(),
            description: "List persisted collaboration artifacts (id, name, size, author). \
                          Use artifact_read with an id to load its full content."
                .into(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }

    fn side_effect_class(&self) -> crate::tools::ToolSideEffect {
        crate::tools::ToolSideEffect::ReadOnly
    }

    async fn execute(&self, _arguments: Value) -> Result<String> {
        // The index is lazy: a fresh process may not have dispatched an agent
        // yet, so rehydrate before listing rather than reporting an empty set.
        self.store.ensure_restored().await;
        let metas = self.store.list().await;
        if metas.is_empty() {
            return Ok("No artifacts persisted yet.".to_string());
        }
        let mut lines = vec![format!("Persisted artifacts ({}):", metas.len())];
        for m in &metas {
            lines.push(format!(
                "  {}  {}  {} bytes  by {}",
                m.id, m.name, m.size_bytes, m.author
            ));
        }
        Ok(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;
    use crate::storage::StorageBackend;
    use crate::tools::Tool;

    // The local backend does not own its root dir, so each test keeps the
    // TempDir alive alongside it (dropping the dir would strand the files).
    fn backend_with_dir() -> (Arc<dyn StorageBackend>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let b: Arc<dyn StorageBackend> = Arc::new(crate::storage::LocalStorageBackend::new(
            dir.path().to_path_buf(),
        ));
        (b, dir)
    }

    #[tokio::test]
    async fn put_and_get_round_trip() {
        let (backend, _dir) = backend_with_dir();
        let store = ArtifactStore::new(backend);
        let meta = store
            .put("the full report body", "report", "worker-a")
            .await
            .unwrap();
        assert!(meta.id.starts_with("art-"));
        assert_eq!(meta.size_bytes, "the full report body".len());
        assert_eq!(meta.name, "report");
        assert_eq!(meta.author, "worker-a");
        let body = store.get(&meta.id).await.unwrap();
        assert_eq!(body, "the full report body");
    }

    #[tokio::test]
    async fn get_unknown_id_is_not_found() {
        let (backend, _dir) = backend_with_dir();
        let store = ArtifactStore::new(backend);
        let err = store.get("art-doesnotexist").await.unwrap_err();
        assert!(matches!(err, Error::NotFound(_)));
    }

    #[tokio::test]
    async fn get_rejects_path_traversal() {
        let (backend, _dir) = backend_with_dir();
        let store = ArtifactStore::new(backend);
        for bad in ["../escape", "a/b", ".."] {
            let err = store.get(bad).await.unwrap_err();
            assert!(
                matches!(err, Error::BadToolArgs { .. }),
                "id '{bad}' must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn artifacts_survive_a_fresh_store_via_restore() {
        let (backend, _dir) = backend_with_dir();
        let store = ArtifactStore::new(backend.clone());
        let meta = store.put("payload", "notes", "w1").await.unwrap();

        let revived = ArtifactStore::new(backend);
        assert!(revived.list().await.is_empty(), "fresh store: no index");
        assert_eq!(revived.get(&meta.id).await.unwrap(), "payload");
        revived.restore().await;
        let listed = revived.list().await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, meta.id);
        assert_eq!(listed[0].name, "notes");
    }

    #[tokio::test]
    async fn ensure_restored_rehydrates_index_once() {
        let (backend, _dir) = backend_with_dir();
        let store = ArtifactStore::new(backend.clone());
        let meta = store.put("payload", "notes", "w1").await.unwrap();

        let revived = ArtifactStore::new(backend);
        assert!(revived.list().await.is_empty());
        revived.ensure_restored().await;
        assert_eq!(revived.list().await.len(), 1);
        // A repeated call is a no-op and must not clear the index.
        revived.ensure_restored().await;
        assert_eq!(revived.list().await.len(), 1);
        assert_eq!(revived.get(&meta.id).await.unwrap(), "payload");
    }

    #[tokio::test]
    async fn list_tracks_live_writes_without_restore() {
        let (backend, _dir) = backend_with_dir();
        let store = ArtifactStore::new(backend);
        assert!(store.list().await.is_empty());
        store.put("a", "one", "w").await.unwrap();
        store.put("bb", "two", "w").await.unwrap();
        let listed = store.list().await;
        assert_eq!(listed.len(), 2);
        let mut names: Vec<&str> = listed.iter().map(|m| m.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["one", "two"]);
    }

    #[test]
    fn reference_text_small_body_is_inlined_in_full() {
        let meta = ArtifactMeta {
            id: "art-abc".into(),
            name: "n".into(),
            author: "w".into(),
            size_bytes: 5,
            created_at: 0,
        };
        let text = artifact_reference_text(&meta, "hello");
        assert!(text.contains("[artifact saved: id=art-abc"));
        assert!(text.contains("artifact_read"));
        assert!(text.ends_with("hello"), "small body must be inlined");
    }

    #[test]
    fn reference_text_large_body_is_head_and_tail_preview() {
        let meta = ArtifactMeta {
            id: "art-big".into(),
            name: "n".into(),
            author: "w".into(),
            size_bytes: ARTIFACT_INLINE_BUDGET_BYTES + 10_000,
            created_at: 0,
        };
        let mut body = "H".repeat(ARTIFACT_INLINE_BUDGET_BYTES + 5_000);
        body.push_str("MIDDLE-PADDING");
        body.push_str(&"T".repeat(5_000));
        let text = artifact_reference_text(&meta, &body);
        assert!(text.contains("first 2048 bytes"), "got: {text}");
        assert!(text.contains("last 256 bytes"));
        assert!(text.contains("artifact_read"), "must teach the loader tool");
        // The full body must NOT be inlined: the middle padding is absent.
        assert!(!text.contains("MIDDLE-PADDING"));
        // Tail preview is present.
        assert!(text.contains("TTT"));
    }

    #[tokio::test]
    async fn put_failure_propagates_not_advertised() {
        struct FailingBackend;
        #[async_trait::async_trait]
        impl StorageBackend for FailingBackend {
            async fn load_transcript(
                &self,
                _session_id: &str,
            ) -> crate::error::Result<Vec<Message>> {
                Ok(vec![])
            }
            async fn save_transcript(
                &self,
                _session_id: &str,
                _messages: &[Message],
            ) -> crate::error::Result<()> {
                Err(crate::error::Error::Storage {
                    message: "boom".into(),
                })
            }
            async fn delete_transcript(&self, _session_id: &str) -> crate::error::Result<()> {
                Err(crate::error::Error::Storage {
                    message: "boom".into(),
                })
            }
            async fn load_memory(&self, _key: &str) -> crate::error::Result<Option<String>> {
                Ok(None)
            }
            async fn save_memory(&self, _key: &str, _value: &str) -> crate::error::Result<()> {
                Err(crate::error::Error::Storage {
                    message: "boom".into(),
                })
            }
            async fn delete_memory(&self, _key: &str) -> crate::error::Result<()> {
                Err(crate::error::Error::Storage {
                    message: "boom".into(),
                })
            }
        }
        let store = ArtifactStore::new(Arc::new(FailingBackend));
        let err = store.put("body", "n", "w").await.unwrap_err();
        assert!(
            matches!(err, Error::Storage { .. }),
            "a failed artifact save must surface as Err, never as a saved reference"
        );
    }

    #[tokio::test]
    async fn artifact_read_tool_executes() {
        let (backend, _dir) = backend_with_dir();
        let store = Arc::new(ArtifactStore::new(backend));
        let meta = store.put("tool-visible body", "n", "w").await.unwrap();
        let tool = ArtifactReadTool::new(store.clone());
        let out = tool.execute(json!({ "id": meta.id })).await.unwrap();
        assert_eq!(out, "tool-visible body");
    }

    #[tokio::test]
    async fn artifact_read_tool_missing_id_errors() {
        let (backend, _dir) = backend_with_dir();
        let tool = ArtifactReadTool::new(Arc::new(ArtifactStore::new(backend)));
        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(matches!(err, Error::BadToolArgs { .. }));
    }

    #[tokio::test]
    async fn artifact_read_tool_readonly() {
        let (backend, _dir) = backend_with_dir();
        let tool = ArtifactReadTool::new(Arc::new(ArtifactStore::new(backend)));
        assert_eq!(
            tool.side_effect_class(),
            crate::tools::ToolSideEffect::ReadOnly
        );
    }

    #[tokio::test]
    async fn artifact_list_tool_empty_and_nonempty() {
        let (backend, _dir) = backend_with_dir();
        let store = Arc::new(ArtifactStore::new(backend));
        let tool = ArtifactListTool::new(store.clone());
        let out = tool.execute(json!({})).await.unwrap();
        assert!(out.contains("No artifacts"), "got: {out}");

        store.put("x", "alpha", "w").await.unwrap();
        let out = tool.execute(json!({})).await.unwrap();
        assert!(out.contains("alpha"), "got: {out}");
        assert!(out.contains("Persisted artifacts (1)"), "got: {out}");
    }

    #[tokio::test]
    async fn artifact_list_tool_rehydrates_index_on_a_fresh_process() {
        let (backend, _dir) = backend_with_dir();
        let store = ArtifactStore::new(backend.clone());
        store.put("body", "restored-report", "w1").await.unwrap();

        // Fresh process: the tool must rehydrate the persisted index before
        // listing instead of reporting an empty set.
        let tool = ArtifactListTool::new(Arc::new(ArtifactStore::new(backend)));
        let out = tool.execute(json!({})).await.unwrap();
        assert!(out.contains("restored-report"), "got: {out}");
    }
}
