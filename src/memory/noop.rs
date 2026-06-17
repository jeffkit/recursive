//! No-op / fallback implementations of [`EmbeddingProvider`] and
//! [`VectorStore`].
//!
//! `NoopEmbedding` always returns an empty vector, signalling to the store
//! that keyword search should be used instead of cosine similarity.
//!
//! `NoopVectorStore` keeps entries in memory and performs case-insensitive
//! substring search — identical to the original `recall` tool behaviour.

use std::sync::Mutex;

use async_trait::async_trait;

use super::{EmbeddingProvider, MemoryEntry, VectorStore};
use crate::error::Result;

// ──────────────────────────────────────────────────────────────────────────────
// NoopEmbedding
// ──────────────────────────────────────────────────────────────────────────────

/// An [`EmbeddingProvider`] that returns an empty vector for every input.
///
/// When paired with [`NoopVectorStore`] this causes the store to fall back to
/// linear keyword search, preserving backward-compatible behaviour.
pub struct NoopEmbedding;

#[async_trait]
impl EmbeddingProvider for NoopEmbedding {
    async fn embed(&self, _text: &str) -> Vec<f32> {
        vec![]
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// NoopVectorStore
// ──────────────────────────────────────────────────────────────────────────────

/// An in-memory [`VectorStore`] that ignores embedding vectors and performs
/// case-insensitive substring search on `query_text`.
///
/// This provides backward-compatible `recall` behaviour with no new
/// dependencies. The store is **not** persisted across process restarts —
/// for durability use `SqliteVecStore` or a cloud-backed store.
pub struct NoopVectorStore {
    entries: Mutex<Vec<MemoryEntry>>,
}

impl NoopVectorStore {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
        }
    }
}

impl Default for NoopVectorStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl VectorStore for NoopVectorStore {
    async fn upsert(&self, entry: &MemoryEntry, _vector: Vec<f32>) -> Result<()> {
        #[allow(clippy::unwrap_used, reason = "mutex poison is unrecoverable")]
        let mut entries = self.entries.lock().unwrap();
        if let Some(existing) = entries.iter_mut().find(|e| e.id == entry.id) {
            *existing = entry.clone();
        } else {
            entries.push(entry.clone());
        }
        Ok(())
    }

    async fn search(
        &self,
        _query_vec: Vec<f32>,
        query_text: &str,
        limit: usize,
    ) -> Result<Vec<MemoryEntry>> {
        #[allow(clippy::unwrap_used, reason = "mutex poison is unrecoverable")]
        let entries = self.entries.lock().unwrap();
        let q = query_text.to_lowercase();
        let matches: Vec<MemoryEntry> = entries
            .iter()
            .filter(|e| {
                e.text.to_lowercase().contains(&q)
                    || e.tags.iter().any(|t| t.to_lowercase().contains(&q))
            })
            .take(limit)
            .cloned()
            .collect();
        Ok(matches)
    }

    async fn remove(&self, id: &str) -> Result<()> {
        #[allow(clippy::unwrap_used, reason = "mutex poison is unrecoverable")]
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|e| e.id != id);
        Ok(())
    }

    async fn list_all(&self) -> Result<Vec<MemoryEntry>> {
        #[allow(clippy::unwrap_used, reason = "mutex poison is unrecoverable")]
        let entries = self.entries.lock().unwrap();
        Ok(entries.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_embedding_returns_empty() {
        let emb = NoopEmbedding;
        assert!(emb.embed("hello world").await.is_empty());
    }

    #[tokio::test]
    async fn noop_store_upsert_and_search() {
        let store = NoopVectorStore::new();

        let entry = MemoryEntry {
            id: "N1".into(),
            text: "Rust is a systems language".into(),
            tags: vec!["rust".into()],
            ts: "2026-01-01T00:00:00Z".into(),
        };
        store.upsert(&entry, vec![]).await.unwrap();

        let results = store.search(vec![], "systems", 5).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "N1");

        let no_results = store.search(vec![], "python", 5).await.unwrap();
        assert!(no_results.is_empty());
    }

    #[tokio::test]
    async fn noop_store_overwrite_on_same_id() {
        let store = NoopVectorStore::new();

        let e1 = MemoryEntry {
            id: "N1".into(),
            text: "original".into(),
            tags: vec![],
            ts: "2026-01-01T00:00:00Z".into(),
        };
        let e2 = MemoryEntry {
            id: "N1".into(),
            text: "updated".into(),
            tags: vec![],
            ts: "2026-01-02T00:00:00Z".into(),
        };
        store.upsert(&e1, vec![]).await.unwrap();
        store.upsert(&e2, vec![]).await.unwrap();

        let all = store.list_all().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].text, "updated");
    }

    #[tokio::test]
    async fn noop_store_remove_by_id() {
        let store = NoopVectorStore::new();
        let e = MemoryEntry {
            id: "N1".into(),
            text: "to be deleted".into(),
            tags: vec![],
            ts: "2026-01-01T00:00:00Z".into(),
        };
        store.upsert(&e, vec![]).await.unwrap();
        store.remove("N1").await.unwrap();
        assert!(store.list_all().await.unwrap().is_empty());
    }
}
