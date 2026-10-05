//! Vector memory layer — semantic storage and retrieval of agent memories.
//!
//! This module provides two traits:
//!
//! - [`EmbeddingProvider`] — converts text into a dense float vector.
//! - [`VectorStore`] — stores and retrieves [`MemoryEntry`] items by semantic
//!   similarity (cosine) or by fallback linear text scan.
//!
//! ## Default (no extra features)
//!
//! [`NoopEmbedding`] and [`NoopVectorStore`] are always available and
//! provide backward-compatible keyword search without any new dependencies.
//!
//! ## OpenAI embeddings (`openai-embedding` feature)
//!
//! [`OpenAiEmbedding`] calls the OpenAI `text-embedding-3-small` endpoint,
//! configured by `RECURSIVE_EMBEDDING_*` with the shared
//! `RECURSIVE_API_KEY` / `RECURSIVE_API_BASE` as fallback.
//!
//! ## SQLite vector store (`vector-memory` feature)
//!
//! [`SqliteVecStore`] persists vectors in a per-workspace SQLite database.
//! Cosine similarity is computed in Rust (linear scan over stored BLOBs),
//! requiring no native extension and no C compiler beyond the bundled SQLite.
//!
//! ## Production wiring
//!
//! [`default_backends`] is the single assembly point used by the tool registry
//! and the CLI: it returns the durable pair when both the feature and an
//! embedding credential are present, and the no-op pair otherwise.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub mod noop;

#[cfg(feature = "openai-embedding")]
pub mod openai_embedding;

#[cfg(feature = "vector-memory")]
pub mod sqlite_vec;

pub use noop::{NoopEmbedding, NoopVectorStore};

#[cfg(feature = "openai-embedding")]
pub use openai_embedding::OpenAiEmbedding;

#[cfg(feature = "vector-memory")]
pub use sqlite_vec::SqliteVecStore;

// ──────────────────────────────────────────────────────────────────────────────
// MemoryEntry
// ──────────────────────────────────────────────────────────────────────────────

/// A single memory fragment that can be stored and retrieved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    /// Unique, stable identifier (e.g. `"N1"`, a UUID, or a content hash).
    pub id: String,
    /// Free-form text content.
    pub text: String,
    /// Optional semantic tags for filtering.
    #[serde(default)]
    pub tags: Vec<String>,
    /// ISO-8601 creation timestamp.
    pub ts: String,
}

// ──────────────────────────────────────────────────────────────────────────────
// EmbeddingProvider
// ──────────────────────────────────────────────────────────────────────────────

/// Converts text into a dense embedding vector.
///
/// Implementations must be [`Send`] + [`Sync`] so they can be shared across
/// async tasks. Return an empty `Vec` to signal "no embedding available"
/// (the store will fall back to linear text search in that case).
#[async_trait]
pub trait EmbeddingProvider: Send + Sync + 'static {
    /// Embed `text` and return a float vector. May return an empty vec on
    /// error or when embedding is intentionally disabled.
    async fn embed(&self, text: &str) -> Vec<f32>;
}

// ──────────────────────────────────────────────────────────────────────────────
// VectorStore
// ──────────────────────────────────────────────────────────────────────────────

/// Persistent store for [`MemoryEntry`] items with optional semantic search.
///
/// All methods are async and must not panic; they return `Result` so the
/// caller can log warnings and continue rather than crashing the agent loop.
#[async_trait]
pub trait VectorStore: Send + Sync + 'static {
    /// Persist a memory entry. If an entry with the same `id` already exists
    /// it should be overwritten.
    async fn upsert(&self, entry: &MemoryEntry, vector: Vec<f32>) -> crate::error::Result<()>;

    /// Retrieve up to `limit` entries whose vector is closest to `query_vec`
    /// (cosine similarity). If `query_vec` is empty, fall back to returning
    /// recent entries in insertion order.
    ///
    /// When `tag` is set, only entries carrying that exact tag are eligible,
    /// and the filter runs *before* the `limit` cut-off — truncating first
    /// would return fewer tagged notes than the caller asked for.
    async fn search(
        &self,
        query_vec: Vec<f32>,
        query_text: &str,
        tag: Option<&str>,
        limit: usize,
    ) -> crate::error::Result<Vec<MemoryEntry>>;

    /// Remove the entry with the given `id`. No-op if not found.
    async fn remove(&self, id: &str) -> crate::error::Result<()>;

    /// Return all entries in insertion order.
    async fn list_all(&self) -> crate::error::Result<Vec<MemoryEntry>>;
}

/// True when `entry` carries `tag`, or when no tag filter was requested.
///
/// Shared by the [`VectorStore`] implementations so "filtered by tag" means the
/// same thing in both.
pub(crate) fn matches_tag(entry: &MemoryEntry, tag: Option<&str>) -> bool {
    tag.map_or(true, |t| entry.tags.iter().any(|et| et == t))
}

// ──────────────────────────────────────────────────────────────────────────────
// default_backends
// ──────────────────────────────────────────────────────────────────────────────

/// Assemble the vector-memory backends for `workspace`.
///
/// This is the production wiring point for `remember` / `recall` / `forget`:
/// without it those tools silently fall back to keyword-only search over
/// `memory.json`.
///
/// Returns the durable pair — a per-workspace `SqliteVecStore` plus an
/// `OpenAiEmbedding` — when the `vector-memory` feature is compiled in *and*
/// an embedding credential is configured (`RECURSIVE_EMBEDDING_API_KEY`, falling
/// back to `RECURSIVE_API_KEY`). Otherwise returns the [`NoopEmbedding`] /
/// [`NoopVectorStore`] pair, which degrades `recall` to the keyword path
/// instead of failing.
///
/// Construction is side-effect free: the SQLite file is created on first write,
/// so a registry that never calls a memory tool leaves no
/// `.recursive/memory_vectors.db` behind. A store that cannot be opened
/// (read-only workspace, …) surfaces as a per-call warning from the tool, which
/// falls back to the keyword path.
pub fn default_backends(
    workspace: &std::path::Path,
) -> (Arc<dyn VectorStore>, Arc<dyn EmbeddingProvider>) {
    #[cfg(feature = "vector-memory")]
    {
        if let Some(embedding) = embedding_from_env() {
            return (
                Arc::new(SqliteVecStore::for_workspace(workspace)),
                embedding,
            );
        }
    }
    #[cfg(not(feature = "vector-memory"))]
    let _ = workspace;
    (Arc::new(NoopVectorStore::new()), Arc::new(NoopEmbedding))
}

#[cfg(feature = "vector-memory")]
fn embedding_from_env() -> Option<Arc<dyn EmbeddingProvider>> {
    OpenAiEmbedding::from_env().map(|e| Arc::new(e) as Arc<dyn EmbeddingProvider>)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn default_backends_return_a_working_store() {
        // Whichever branch is taken (feature off, or no embedding key) the pair
        // must be a functioning store, not a panic. The empty vector keeps the
        // test offline — it is the documented "no embedding available, use
        // keyword search" state.
        let dir = tempfile::tempdir().expect("tempdir");
        let (store, _embedding) = default_backends(dir.path());

        let entry = MemoryEntry {
            id: "N1".into(),
            text: "hello world".into(),
            tags: vec!["greeting".into()],
            ts: "2026-01-01T00:00:00Z".into(),
        };
        store.upsert(&entry, vec![]).await.expect("upsert");
        assert_eq!(store.list_all().await.expect("list_all").len(), 1);

        let hits = store
            .search(vec![], "hello", None, 10)
            .await
            .expect("search");
        assert_eq!(hits.len(), 1, "keyword search must find the entry");
        assert_eq!(hits[0].id, "N1");

        store.remove("N1").await.expect("remove");
        assert!(
            store.list_all().await.expect("list_all").is_empty(),
            "removed entry must be gone"
        );
    }
}
