//! SQLite-backed vector store using pure `rusqlite`.
//!
//! Vectors are stored as BLOB (little-endian f32 bytes). Cosine similarity is
//! computed in Rust by loading all vectors and scanning — suitable for typical
//! agent memory sizes (< 10 000 entries) without requiring a C extension.
//!
//! For larger corpora consider switching to a dedicated vector database; the
//! [`VectorStore`] trait makes that a drop-in swap.
//!
//! # Feature flag
//!
//! This module is compiled when `--features vector-memory` is passed.
//! Without the feature the `NoopVectorStore` is used as fallback.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use rusqlite::Connection;

use super::{MemoryEntry, VectorStore};
use crate::error::{Error, Result};

/// Shorthand: wrap a rusqlite or IO error as `Error::Storage`.
fn storage_err(e: impl std::fmt::Display) -> Error {
    Error::Storage {
        message: e.to_string(),
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Convert `Vec<f32>` to little-endian bytes.
fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Convert little-endian bytes back to `Vec<f32>`.
fn blob_to_vec(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
        .collect()
}

/// Cosine similarity between two equal-length vectors. Returns 0 if either
/// has zero magnitude (avoids division by zero).
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let mag_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let mag_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if mag_a == 0.0 || mag_b == 0.0 {
        return 0.0;
    }
    dot / (mag_a * mag_b)
}

// ──────────────────────────────────────────────────────────────────────────────
// SqliteVecStore
// ──────────────────────────────────────────────────────────────────────────────

/// A [`VectorStore`] backed by SQLite with in-process cosine similarity search.
///
/// Schema:
/// ```sql
/// CREATE TABLE IF NOT EXISTS memory_entries (
///   id        TEXT PRIMARY KEY,
///   text      TEXT NOT NULL,
///   tags      TEXT NOT NULL,   -- JSON array
///   ts        TEXT NOT NULL,
///   embedding BLOB             -- NULL when embedding was unavailable
/// );
/// ```
pub struct SqliteVecStore {
    db: Mutex<Connection>,
}

impl SqliteVecStore {
    /// Open (or create) the SQLite database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| storage_err(e.to_string()))?;
        }
        let conn =
            Connection::open(path).map_err(|e| storage_err(format!("sqlite open failed: {e}")))?;
        Self::init_schema(&conn)?;
        Ok(Self {
            db: Mutex::new(conn),
        })
    }

    /// Convenience: open a store at the default path for `workspace`.
    pub fn for_workspace(workspace: impl Into<PathBuf>) -> Result<Self> {
        let mut path = workspace.into();
        path.push(".recursive");
        path.push("memory_vectors.db");
        Self::open(path)
    }

    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS memory_entries (
               id        TEXT PRIMARY KEY,
               text      TEXT NOT NULL,
               tags      TEXT NOT NULL,
               ts        TEXT NOT NULL,
               embedding BLOB
             );",
        )
        .map_err(|e| storage_err(format!("schema init failed: {e}")))?;
        Ok(())
    }

    fn row_to_entry(id: String, text: String, tags_json: String, ts: String) -> MemoryEntry {
        let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
        MemoryEntry { id, text, tags, ts }
    }
}

#[async_trait]
impl VectorStore for SqliteVecStore {
    async fn upsert(&self, entry: &MemoryEntry, vector: Vec<f32>) -> Result<()> {
        let conn = self.db.lock().unwrap();
        let tags_json = serde_json::to_string(&entry.tags).unwrap_or_else(|_| "[]".into());
        let blob = if vector.is_empty() {
            None
        } else {
            Some(vec_to_blob(&vector))
        };
        conn.execute(
            "INSERT OR REPLACE INTO memory_entries (id, text, tags, ts, embedding)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![entry.id, entry.text, tags_json, entry.ts, blob],
        )
        .map_err(|e| storage_err(e.to_string()))?;
        Ok(())
    }

    async fn search(
        &self,
        query_vec: Vec<f32>,
        query_text: &str,
        limit: usize,
    ) -> Result<Vec<MemoryEntry>> {
        let conn = self.db.lock().unwrap();

        if !query_vec.is_empty() {
            // Load all rows with embeddings and rank by cosine similarity.
            let mut stmt = conn
                .prepare(
                    "SELECT id, text, tags, ts, embedding FROM memory_entries
                     WHERE embedding IS NOT NULL",
                )
                .map_err(|e| storage_err(e.to_string()))?;

            let mut scored: Vec<(f32, MemoryEntry)> = stmt
                .query_map([], |row| {
                    let blob: Vec<u8> = row.get(4)?;
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        blob,
                    ))
                })
                .map_err(|e| storage_err(e.to_string()))?
                .filter_map(|r| r.ok())
                .map(|(id, text, tags_json, ts, blob)| {
                    let vec = blob_to_vec(&blob);
                    let score = cosine_similarity(&query_vec, &vec);
                    let entry = Self::row_to_entry(id, text, tags_json, ts);
                    (score, entry)
                })
                .collect();

            // Sort by descending similarity.
            scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            let entries: Vec<MemoryEntry> =
                scored.into_iter().take(limit).map(|(_, e)| e).collect();
            return Ok(entries);
        }

        // Fallback: keyword scan.
        let q = format!("%{}%", query_text.to_lowercase());
        let mut stmt = conn
            .prepare(
                "SELECT id, text, tags, ts FROM memory_entries
                 WHERE lower(text) LIKE ?1
                 ORDER BY ts DESC
                 LIMIT ?2",
            )
            .map_err(|e| storage_err(e.to_string()))?;

        let entries: Vec<MemoryEntry> = stmt
            .query_map(rusqlite::params![q, limit as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(|e| storage_err(e.to_string()))?
            .filter_map(|r| r.ok())
            .map(|(id, text, tags_json, ts)| Self::row_to_entry(id, text, tags_json, ts))
            .collect();
        Ok(entries)
    }

    async fn remove(&self, id: &str) -> Result<()> {
        let conn = self.db.lock().unwrap();
        conn.execute("DELETE FROM memory_entries WHERE id = ?1", [id])
            .map_err(|e| storage_err(e.to_string()))?;
        Ok(())
    }

    async fn list_all(&self) -> Result<Vec<MemoryEntry>> {
        let conn = self.db.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, text, tags, ts FROM memory_entries ORDER BY ts")
            .map_err(|e| storage_err(e.to_string()))?;
        let entries: Vec<MemoryEntry> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(|e| storage_err(e.to_string()))?
            .filter_map(|r| r.ok())
            .map(|(id, text, tags_json, ts)| Self::row_to_entry(id, text, tags_json, ts))
            .collect();
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn sqlite_store_upsert_and_list() {
        let dir = TempDir::new().unwrap();
        let store = SqliteVecStore::open(dir.path().join("test.db")).unwrap();

        let entry = MemoryEntry {
            id: "E1".into(),
            text: "hello world".into(),
            tags: vec!["test".into()],
            ts: "2026-01-01T00:00:00Z".into(),
        };
        store.upsert(&entry, vec![0.1, 0.2, 0.3]).await.unwrap();

        let all = store.list_all().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, "E1");
    }

    #[tokio::test]
    async fn sqlite_store_vector_search() {
        let dir = TempDir::new().unwrap();
        let store = SqliteVecStore::open(dir.path().join("test.db")).unwrap();

        // Insert two entries with different vectors.
        let e1 = MemoryEntry {
            id: "E1".into(),
            text: "Rust".into(),
            tags: vec![],
            ts: "2026-01-01T00:00:00Z".into(),
        };
        let e2 = MemoryEntry {
            id: "E2".into(),
            text: "Python".into(),
            tags: vec![],
            ts: "2026-01-01T00:00:01Z".into(),
        };
        // E1 vector close to query; E2 orthogonal.
        store.upsert(&e1, vec![1.0, 0.0, 0.0]).await.unwrap();
        store.upsert(&e2, vec![0.0, 1.0, 0.0]).await.unwrap();

        let results = store.search(vec![1.0, 0.0, 0.0], "", 2).await.unwrap();
        assert_eq!(results[0].id, "E1", "E1 should rank first");
    }

    #[tokio::test]
    async fn sqlite_store_keyword_fallback() {
        let dir = TempDir::new().unwrap();
        let store = SqliteVecStore::open(dir.path().join("test.db")).unwrap();

        let e = MemoryEntry {
            id: "E1".into(),
            text: "Rust is fast".into(),
            tags: vec![],
            ts: "2026-01-01T00:00:00Z".into(),
        };
        store.upsert(&e, vec![]).await.unwrap();

        // Empty query_vec triggers keyword fallback.
        let results = store.search(vec![], "fast", 5).await.unwrap();
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn sqlite_store_remove() {
        let dir = TempDir::new().unwrap();
        let store = SqliteVecStore::open(dir.path().join("test.db")).unwrap();
        let e = MemoryEntry {
            id: "E1".into(),
            text: "delete me".into(),
            tags: vec![],
            ts: "2026-01-01T00:00:00Z".into(),
        };
        store.upsert(&e, vec![0.1, 0.2, 0.3]).await.unwrap();
        store.remove("E1").await.unwrap();
        assert!(store.list_all().await.unwrap().is_empty());
    }

    #[test]
    fn cosine_similarity_basic() {
        assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-5);
        assert!((cosine_similarity(&[1.0, 0.0], &[0.0, 1.0])).abs() < 1e-5);
    }
}
