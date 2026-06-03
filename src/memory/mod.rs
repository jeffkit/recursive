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
//! [`OpenAiEmbedding`] calls the OpenAI `text-embedding-3-small` endpoint.
//! Reuses the existing `RECURSIVE_API_KEY` / `RECURSIVE_API_BASE` env vars.
//!
//! ## SQLite vector store (`vector-memory` feature)
//!
//! [`SqliteVecStore`] persists vectors in a per-workspace SQLite database.
//! Cosine similarity is computed in Rust (linear scan over stored BLOBs),
//! requiring no native extension and no C compiler beyond the bundled SQLite.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

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
    async fn search(
        &self,
        query_vec: Vec<f32>,
        query_text: &str,
        limit: usize,
    ) -> crate::error::Result<Vec<MemoryEntry>>;

    /// Remove the entry with the given `id`. No-op if not found.
    async fn remove(&self, id: &str) -> crate::error::Result<()>;

    /// Return all entries in insertion order.
    async fn list_all(&self) -> crate::error::Result<Vec<MemoryEntry>>;
}
