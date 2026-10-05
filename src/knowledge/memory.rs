//! Persistent memory tools: `remember`, `recall`, `forget`.
//!
//! Also provides a scratchpad (working memory) layer: `scratchpad_set`,
//! `scratchpad_get`, `scratchpad_delete`, `scratchpad_list`. The scratchpad
//! is stored in `<workspace>/.recursive/scratchpad.json` and its contents
//! are injected into the system prompt as a summary.
//!
//! Notes are stored in `<workspace>/.recursive/memory.json` (or
//! `~/.recursive/memory.json` if `RECURSIVE_MEMORY_GLOBAL=1`).
//! Schema:
//! ```json
//! { "notes": [ { "id": "N1", "tags": ["rust"], "text": "...", "ts": "..." } ] }
//! ```
//!
//! Writes go through `crate::atomic::atomic_write` so a crash mid-write can
//! never leave a truncated `memory.json`. Re-remembering identical text
//! refreshes that note instead of appending a duplicate, and the store is
//! capped at [`memory_max_notes`] entries (`RECURSIVE_MEMORY_MAX_NOTES`,
//! default 1000) with the oldest notes evicted first — so the file cannot grow
//! without bound, and evicted notes are dropped from the vector index too.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::memory::{EmbeddingProvider, MemoryEntry, NoopEmbedding, NoopVectorStore, VectorStore};
use crate::tools::Tool;

/// A single memory note.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    pub id: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub text: String,
    pub ts: String,
}

/// The on-disk store.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MemoryStore {
    pub notes: Vec<Note>,
}

impl MemoryStore {
    /// Load from a path, returning an empty store if the file doesn't exist.
    fn load(path: &std::path::Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path).map_err(|e| Error::Tool {
            name: "memory".into(),
            call_id: None,
            message: format!("failed to read memory file: {e}"),
        })?;
        serde_json::from_str(&raw).map_err(|e| Error::Tool {
            name: "memory".into(),
            call_id: None,
            message: format!("malformed memory file: {e}"),
        })
    }

    /// Save to disk, creating parent directories if needed.
    ///
    /// Uses the atomic write-then-rename helper: concurrent `remember` calls,
    /// or a crash mid-write, must never leave a half-written/malformed
    /// `memory.json` behind (a corrupt file makes every later `recall` fail).
    fn save(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Tool {
                name: "memory".into(),
                call_id: None,
                message: format!("failed to create memory directory: {e}"),
            })?;
        }
        let raw = serde_json::to_string_pretty(self).map_err(|e| Error::Tool {
            name: "memory".into(),
            call_id: None,
            message: format!("failed to serialize memory: {e}"),
        })?;
        crate::atomic::atomic_write(path, raw.as_bytes()).map_err(|e| Error::Tool {
            name: "memory".into(),
            call_id: None,
            message: format!("failed to write memory file: {e}"),
        })?;
        Ok(())
    }

    /// Generate the next monotonic ID.
    fn next_id(&self) -> String {
        let max = self
            .notes
            .iter()
            .filter_map(|n| n.id.strip_prefix('N'))
            .filter_map(|s| s.parse::<u32>().ok())
            .max()
            .unwrap_or(0);
        format!("N{}", max + 1)
    }

    /// Add a note, returning its ID.
    ///
    /// Text that is already stored is not duplicated: the existing note keeps
    /// its ID, gets a fresh timestamp and absorbs any tags it did not have.
    /// It also moves to the newest position — it was just re-confirmed, so it
    /// must not be the first thing [`MemoryStore::enforce_capacity`] throws
    /// away.
    fn add(&mut self, text: String, tags: Vec<String>) -> String {
        if let Some(pos) = self.notes.iter().position(|n| n.text == text) {
            let mut note = self.notes.remove(pos);
            note.ts = chrono_now_rfc3339();
            for tag in tags {
                if !note.tags.contains(&tag) {
                    note.tags.push(tag);
                }
            }
            let id = note.id.clone();
            self.notes.push(note);
            return id;
        }
        let id = self.next_id();
        let ts = chrono_now_rfc3339();
        self.notes.push(Note {
            id: id.clone(),
            tags,
            text,
            ts,
        });
        id
    }

    /// Evict the oldest notes until at most `max` remain, returning their IDs
    /// in eviction order. `max == 0` disables the cap.
    ///
    /// Notes are stored in insertion order, so the front of the vector is the
    /// oldest. Callers must remove the returned IDs from the vector index as
    /// well, or a semantic `recall` would resurrect an evicted note.
    fn enforce_capacity(&mut self, max: usize) -> Vec<String> {
        if max == 0 || self.notes.len() <= max {
            return Vec::new();
        }
        let overflow = self.notes.len() - max;
        self.notes.drain(..overflow).map(|n| n.id).collect()
    }

    /// Remove a note by ID. Returns true if found.
    fn remove(&mut self, id: &str) -> bool {
        let before = self.notes.len();
        self.notes.retain(|n| n.id != id);
        self.notes.len() < before
    }

    /// Search notes by query (case-insensitive substring in text or tags)
    /// or by exact tag match. Returns up to `limit` results, most recent first.
    fn search(&self, query: Option<&str>, tag: Option<&str>, limit: usize) -> Vec<&Note> {
        let mut results: Vec<&Note> = self
            .notes
            .iter()
            .filter(|n| {
                let matches_query = query.map_or(true, |q| {
                    let q_lower = q.to_lowercase();
                    n.text.to_lowercase().contains(&q_lower)
                        || n.tags.iter().any(|t| t.to_lowercase().contains(&q_lower))
                });
                let matches_tag = tag.map_or(true, |t| n.tags.iter().any(|nt| nt == t));
                matches_query && matches_tag
            })
            .collect();
        // Most recent first (reverse chronological)
        results.reverse();
        results.truncate(limit);
        results
    }
}

/// Get an RFC 3339 timestamp string.
fn chrono_now_rfc3339() -> String {
    // Use std::time to build a simple UTC timestamp without chrono dependency.
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    // Format as ISO 8601 / RFC 3339
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;

    // Compute year/month/day from days since epoch (simplified)
    let (year, month, day) = days_to_date(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hours, minutes, seconds
    )
}

/// Convert days since Unix epoch to (year, month, day).
/// Uses a simple leap-year-aware algorithm.
fn days_to_date(mut days: u64) -> (u64, u64, u64) {
    // Start from 1970-01-01
    let mut year: u64 = 1970;
    loop {
        let days_in_year = if is_leap(year) { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }
    let months_days: [u64; 12] = if is_leap(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month: u64 = 1;
    for &md in &months_days {
        if days < md {
            break;
        }
        days -= md;
        month += 1;
    }
    let day = days + 1; // 1-indexed
    (year, month, day)
}

fn is_leap(year: u64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Determine the memory file path based on workspace and env var.
pub fn memory_path(workspace: &std::path::Path) -> PathBuf {
    if std::env::var("RECURSIVE_MEMORY_GLOBAL").as_deref() == Ok("1") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(".recursive").join("memory.json");
        }
    }
    workspace.join(".recursive").join("memory.json")
}

/// Maximum number of notes kept in the store, from
/// `RECURSIVE_MEMORY_MAX_NOTES` (default 1000). `0` disables the cap.
pub fn memory_max_notes() -> usize {
    std::env::var("RECURSIVE_MEMORY_MAX_NOTES")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(1000)
}

/// Load the memory store from the workspace-relative path.
pub fn load_memory(workspace: &std::path::Path) -> Result<MemoryStore> {
    let path = memory_path(workspace);
    MemoryStore::load(&path)
}

/// Build a memory summary string for injection into the system prompt.
/// Returns the top N most recent notes as an index-style listing, or empty
/// string if no notes exist.
///
/// The output format mimics fake-cc's MEMORY.md index style:
/// `- [ID](recall) — one-line hook`
/// This keeps the summary compact and token-efficient while guiding the
/// agent to use `recall` for full content when needed.
pub fn memory_summary(workspace: &std::path::Path, limit: usize) -> String {
    let store = match load_memory(workspace) {
        Ok(s) => s,
        Err(_) => return String::new(),
    };
    if store.notes.is_empty() {
        return String::new();
    }
    let mut lines: Vec<String> = Vec::new();
    lines.push("# Memory Index".to_string());
    lines.push(format!(
        "Top {} most recent notes. Use `recall` with the note ID to read full content.",
        limit
    ));
    // Most recent first
    let mut notes: Vec<&Note> = store.notes.iter().collect();
    notes.reverse();
    for note in notes.iter().take(limit) {
        // Generate a concise "hook" from the note text
        let hook = if note.text.len() > 80 {
            format!("{}...", crate::truncate_str(&note.text, 77))
        } else {
            note.text.clone()
        };
        // Replace newlines with spaces to keep the index compact
        let hook = hook.replace('\n', " ");
        // Index-style format: `- [ID](recall) — hook`
        // The "(recall)" indicates the tool to use for full content
        lines.push(format!("- [{}](recall) — {}", note.id, hook));
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

/// Render one `recall` result line: `ID [tag,…] text` (tags omitted when none).
fn format_memory_line(id: &str, tags: &[String], text: &str) -> String {
    if tags.is_empty() {
        format!("{id} {text}")
    } else {
        format!("{id} [{}] {text}", tags.join(","))
    }
}

pub struct Remember {
    workspace: PathBuf,
    /// Mutex for thread-safe access to the memory file.
    lock: Mutex<()>,
    /// Maximum number of stored notes; older ones are evicted first.
    max_notes: usize,
    /// Optional vector store for semantic indexing.
    vector_store: Arc<dyn VectorStore>,
    /// Optional embedding provider for generating vectors.
    embedding_provider: Arc<dyn EmbeddingProvider>,
}

impl Remember {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            lock: Mutex::new(()),
            max_notes: memory_max_notes(),
            vector_store: Arc::new(NoopVectorStore::new()),
            embedding_provider: Arc::new(NoopEmbedding),
        }
    }

    /// Override the capacity cap (see [`memory_max_notes`]).
    pub fn with_max_notes(mut self, max: usize) -> Self {
        self.max_notes = max;
        self
    }

    /// Inject a vector store + embedding provider for semantic indexing.
    ///
    /// When set, every `remember` call will also embed the text and upsert the
    /// vector into the store, enabling semantic `recall` queries.
    pub fn with_vector_store(
        mut self,
        store: Arc<dyn VectorStore>,
        embedding: Arc<dyn EmbeddingProvider>,
    ) -> Self {
        self.vector_store = store;
        self.embedding_provider = embedding;
        self
    }
}

#[async_trait]
impl Tool for Remember {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "remember".into(),
            description: "Save a note to persistent memory. The note will be available in future sessions via `recall` or injected into the system prompt.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "The note text to remember"
                    },
                    "tags": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Optional tags for categorising the note"
                    }
                },
                "required": ["text"]
            }),
        }
    }

    fn is_deferred(&self) -> bool {
        true
    }

    fn side_effect_class(&self) -> crate::tools::ToolSideEffect {
        crate::tools::ToolSideEffect::Mutating
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let text = arguments["text"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "remember".into(),
                message: "missing required parameter: text".to_string(),
            })?
            .to_string();

        let tags: Vec<String> = arguments["tags"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let path = memory_path(&self.workspace);
        let mut file_store = MemoryStore::load(&path)?;
        let id = file_store.add(text.clone(), tags);
        let evicted = file_store.enforce_capacity(self.max_notes);
        // Index the tags the file store ended up with, not just the ones this
        // call passed: re-remembering a note merges the new tags into the
        // existing one, and the two stores must not disagree about what a note
        // is tagged with.
        let indexed_tags = file_store
            .notes
            .iter()
            .find(|n| n.id == id)
            .map(|n| n.tags.clone())
            .unwrap_or_default();
        file_store.save(&path)?;
        drop(_guard);

        // Also index in the vector store if one is configured.
        let ts = chrono::Utc::now().to_rfc3339();
        let entry = MemoryEntry {
            id: id.clone(),
            text: text.clone(),
            tags: indexed_tags,
            ts,
        };
        let vector = self.embedding_provider.embed(&text).await;
        if let Err(e) = self.vector_store.upsert(&entry, vector).await {
            tracing::warn!(error = %e, note_id = %id, "remember: vector upsert failed");
        }
        for evicted_id in evicted {
            if let Err(e) = self.vector_store.remove(&evicted_id).await {
                tracing::warn!(error = %e, note_id = %evicted_id, "remember: eviction from vector store failed");
            }
        }

        Ok(format!("saved note {id}"))
    }
}

pub struct Recall {
    workspace: PathBuf,
    /// Optional vector store for semantic retrieval.
    vector_store: Arc<dyn VectorStore>,
    /// Optional embedding provider for query vectorisation.
    embedding_provider: Arc<dyn EmbeddingProvider>,
}

impl Recall {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            vector_store: Arc::new(NoopVectorStore::new()),
            embedding_provider: Arc::new(NoopEmbedding),
        }
    }

    /// Inject a vector store + embedding provider for semantic retrieval.
    ///
    /// When set, `recall` will embed the query and perform cosine-similarity
    /// search instead of keyword substring search.
    pub fn with_vector_store(
        mut self,
        store: Arc<dyn VectorStore>,
        embedding: Arc<dyn EmbeddingProvider>,
    ) -> Self {
        self.vector_store = store;
        self.embedding_provider = embedding;
        self
    }
}

#[async_trait]
impl Tool for Recall {
    fn is_deferred(&self) -> bool {
        true
    }

    fn side_effect_class(&self) -> crate::tools::ToolSideEffect {
        crate::tools::ToolSideEffect::ReadOnly
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "recall".into(),
            description: "Search persistent memory for notes matching a query or tag. Returns up to `limit` results, most recent first.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Case-insensitive substring to search for in note text or tags"
                    },
                    "tag": {
                        "type": "string",
                        "description": "Exact tag to filter by"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of results (default 10)",
                        "default": 10
                    }
                }
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let query = arguments["query"].as_str().unwrap_or("");
        let tag = arguments["tag"].as_str();
        let limit = arguments["limit"].as_i64().unwrap_or(10) as usize;

        // `memory.json` is the durable source of truth: it holds every note
        // ever written, including ones the vector index has never seen (written
        // before the feature was enabled, or while the index was unreachable).
        // Querying it on every call is what keeps `recall` working in the
        // default build, where the shared backends are a no-op — their
        // in-process store must never shadow the file.
        let path = memory_path(&self.workspace);
        let file_store = MemoryStore::load(&path)?;
        let query_opt = if query.is_empty() { None } else { Some(query) };
        let file_hits = file_store.search(query_opt, tag, limit);

        let mut lines: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        // Semantic hits rank by relevance, so they lead when a real query
        // vector is available. An empty query is never embedded (an embedding
        // of "" ranks garbage above real notes, and a tag/limit-only recall
        // must stay a recency listing), and the no-op provider returns an empty
        // vector for every input — both cases skip the semantic path entirely,
        // leaving the file hits to answer the call.
        if !query.is_empty() {
            let query_vec = self.embedding_provider.embed(query).await;
            if !query_vec.is_empty() {
                match self.vector_store.search(query_vec, query, tag, limit).await {
                    Ok(entries) => {
                        for entry in entries {
                            if lines.len() >= limit {
                                break;
                            }
                            if seen.insert(entry.id.clone()) {
                                lines.push(format_memory_line(&entry.id, &entry.tags, &entry.text));
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "recall: vector search failed, falling back to file");
                    }
                }
            }
        }

        // Durable keyword hits fill the remaining slots — and are the whole
        // answer in the default build. De-duplicating by id keeps a note the
        // index already returned from appearing twice.
        for note in file_hits {
            if lines.len() >= limit {
                break;
            }
            if seen.insert(note.id.clone()) {
                lines.push(format_memory_line(&note.id, &note.tags, &note.text));
            }
        }

        if lines.is_empty() {
            return Ok("no matching notes found".to_string());
        }
        Ok(lines.join("\n"))
    }
}

pub struct Forget {
    workspace: PathBuf,
    lock: Mutex<()>,
    /// Optional vector store the note must also be removed from.
    vector_store: Arc<dyn VectorStore>,
}

impl Forget {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            lock: Mutex::new(()),
            vector_store: Arc::new(NoopVectorStore::new()),
        }
    }

    /// Inject the same vector store the note was indexed into, so `forget`
    /// deletes both copies.
    pub fn with_vector_store(mut self, store: Arc<dyn VectorStore>) -> Self {
        self.vector_store = store;
        self
    }
}

#[async_trait]
impl Tool for Forget {
    fn is_deferred(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "forget".into(),
            description: "Remove a note from persistent memory by its ID.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "The ID of the note to remove (e.g. N3)"
                    }
                },
                "required": ["id"]
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let id = arguments["id"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "forget".into(),
                message: "missing required parameter: id".to_string(),
            })?
            .to_string();

        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let path = memory_path(&self.workspace);
        let mut store = MemoryStore::load(&path)?;
        let removed = store.remove(&id);
        if removed {
            store.save(&path)?;
        }
        drop(_guard);

        // Drop the vector copy too: otherwise a later semantic `recall` would
        // resurrect a note the user asked to forget. This is attempted even
        // when `memory.json` had no such id — the two stores can drift.
        if let Err(e) = self.vector_store.remove(&id).await {
            tracing::warn!(error = %e, note_id = %id, "forget: vector removal failed");
        }

        if removed {
            Ok(format!("removed {id}"))
        } else {
            Ok(format!("no such id: {id}"))
        }
    }
}

// ---------------------------------------------------------------------------
// Scratchpad (working memory)
// ---------------------------------------------------------------------------

/// A single scratchpad entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScratchpadEntry {
    pub key: String,
    pub value: String,
}

/// The on-disk scratchpad store.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Scratchpad {
    pub entries: Vec<ScratchpadEntry>,
}

impl Scratchpad {
    /// Load from a path, returning an empty scratchpad if the file doesn't exist.
    fn load(path: &std::path::Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path).map_err(|e| Error::Tool {
            name: "scratchpad".into(),
            call_id: None,
            message: format!("failed to read scratchpad file: {e}"),
        })?;
        serde_json::from_str(&raw).map_err(|e| Error::Tool {
            name: "scratchpad".into(),
            call_id: None,
            message: format!("malformed scratchpad file: {e}"),
        })
    }

    /// Save to disk, creating parent directories if needed.
    fn save(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Tool {
                name: "scratchpad".into(),
                call_id: None,
                message: format!("failed to create scratchpad directory: {e}"),
            })?;
        }
        let raw = serde_json::to_string_pretty(self).map_err(|e| Error::Tool {
            name: "scratchpad".into(),
            call_id: None,
            message: format!("failed to serialize scratchpad: {e}"),
        })?;
        std::fs::write(path, raw).map_err(|e| Error::Tool {
            name: "scratchpad".into(),
            call_id: None,
            message: format!("failed to write scratchpad file: {e}"),
        })?;
        Ok(())
    }

    /// Set a key-value pair (insert or update).
    fn set(&mut self, key: String, value: String) {
        // Replace existing entry with the same key, or add new one.
        if let Some(existing) = self.entries.iter_mut().find(|e| e.key == key) {
            existing.value = value;
        } else {
            self.entries.push(ScratchpadEntry { key, value });
        }
    }

    /// Get the value for a key.
    fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|e| e.key == key)
            .map(|e| e.value.as_str())
    }

    /// Delete an entry by key. Returns true if found.
    fn delete(&mut self, key: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.key != key);
        self.entries.len() < before
    }

    /// List all keys.
    fn keys(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.key.as_str()).collect()
    }
}

/// Determine the scratchpad file path. Lives under the per-user data
/// dir so it doesn't pollute the project tree:
/// `~/.recursive/workspaces/<ws-hash>/scratchpad.json`.
pub fn scratchpad_path(workspace: &std::path::Path) -> PathBuf {
    crate::paths::user_scratchpad_path(workspace)
        .unwrap_or_else(|_| workspace.join(".recursive").join("scratchpad.json"))
}

/// Load the scratchpad from the workspace-relative path.
pub fn load_scratchpad(workspace: &std::path::Path) -> Result<Scratchpad> {
    let path = scratchpad_path(workspace);
    Scratchpad::load(&path)
}

/// Build a scratchpad summary string for injection into the system prompt.
/// Returns a formatted block of all key-value pairs, or empty string if
/// the scratchpad is empty.
pub fn scratchpad_summary(workspace: &std::path::Path) -> String {
    let pad = match load_scratchpad(workspace) {
        Ok(p) => p,
        Err(_) => return String::new(),
    };
    if pad.entries.is_empty() {
        return String::new();
    }
    let mut lines: Vec<String> = Vec::new();
    lines.push("# Working Memory (scratchpad)".to_string());
    for entry in &pad.entries {
        // Truncate long values for the summary
        let value_preview = if entry.value.len() > 200 {
            format!("{}...", crate::truncate_str(&entry.value, 197))
        } else {
            entry.value.clone()
        };
        lines.push(format!("- {}: {}", entry.key, value_preview));
    }
    lines.join("\n")
}

/// Migrate old-format scratchpad data (if any) to the new format.
/// Currently a no-op placeholder for future migration logic.
pub fn migrate_scratchpad(_workspace: &std::path::Path) -> Result<()> {
    // No old format to migrate from yet.
    Ok(())
}

// ---------------------------------------------------------------------------
// WorkingMemoryTool: exposes scratchpad operations as tools
// ---------------------------------------------------------------------------

pub struct WorkingMemoryTool {
    workspace: PathBuf,
    lock: Mutex<()>,
}

impl WorkingMemoryTool {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            lock: Mutex::new(()),
        }
    }
}

#[async_trait]
impl Tool for WorkingMemoryTool {
    fn is_deferred(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "scratchpad_set".into(),
            description: "Store a value in working memory (scratchpad) under a key. Use this to remember intermediate results, decisions, or context across steps. The scratchpad contents are injected into the system prompt.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "key": {
                        "type": "string",
                        "description": "The key to store under"
                    },
                    "value": {
                        "type": "string",
                        "description": "The value to store"
                    }
                },
                "required": ["key", "value"]
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let key = arguments["key"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "scratchpad_set".into(),
                message: "missing required parameter: key".to_string(),
            })?
            .to_string();
        let value = arguments["value"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "scratchpad_set".into(),
                message: "missing required parameter: value".to_string(),
            })?
            .to_string();

        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let path = scratchpad_path(&self.workspace);
        let mut pad = Scratchpad::load(&path)?;
        pad.set(key.clone(), value);
        pad.save(&path)?;
        Ok(format!("scratchpad key '{key}' set"))
    }
}

/// Helper: dispatch scratchpad operations based on the tool name.
/// This is used by the multi-tool approach where one struct handles
/// multiple tool names.
pub struct ScratchpadGet {
    workspace: PathBuf,
}

impl ScratchpadGet {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
        }
    }
}

#[async_trait]
impl Tool for ScratchpadGet {
    fn is_deferred(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "scratchpad_get".into(),
            description: "Retrieve a value from working memory (scratchpad) by key.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "key": {
                        "type": "string",
                        "description": "The key to retrieve"
                    }
                },
                "required": ["key"]
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let key = arguments["key"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "scratchpad_get".into(),
                message: "missing required parameter: key".to_string(),
            })?
            .to_string();

        let path = scratchpad_path(&self.workspace);
        let pad = Scratchpad::load(&path)?;
        match pad.get(&key) {
            Some(value) => Ok(value.to_string()),
            None => Ok(format!("no such key: {key}")),
        }
    }
}

pub struct ScratchpadDelete {
    workspace: PathBuf,
    lock: Mutex<()>,
}

impl ScratchpadDelete {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            lock: Mutex::new(()),
        }
    }
}

#[async_trait]
impl Tool for ScratchpadDelete {
    fn is_deferred(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "scratchpad_delete".into(),
            description: "Delete a key from working memory (scratchpad).".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "key": {
                        "type": "string",
                        "description": "The key to delete"
                    }
                },
                "required": ["key"]
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let key = arguments["key"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "scratchpad_delete".into(),
                message: "missing required parameter: key".to_string(),
            })?
            .to_string();

        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let path = scratchpad_path(&self.workspace);
        let mut pad = Scratchpad::load(&path)?;
        if pad.delete(&key) {
            pad.save(&path)?;
            Ok(format!("scratchpad key '{key}' deleted"))
        } else {
            Ok(format!("no such key: {key}"))
        }
    }
}

pub struct ScratchpadList {
    workspace: PathBuf,
}

impl ScratchpadList {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
        }
    }
}

#[async_trait]
impl Tool for ScratchpadList {
    fn is_deferred(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "scratchpad_list".into(),
            description: "List all keys currently stored in working memory (scratchpad).".into(),
            parameters: json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    async fn execute(&self, _arguments: Value) -> Result<String> {
        let path = scratchpad_path(&self.workspace);
        let pad = Scratchpad::load(&path)?;
        let keys = pad.keys();
        if keys.is_empty() {
            return Ok("scratchpad is empty".to_string());
        }
        Ok(keys.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── MemoryStore helpers ──────────────────────────────────────────────────

    fn fresh_store() -> MemoryStore {
        MemoryStore::default()
    }

    // --- load ---

    #[test]
    fn load_returns_empty_store_when_file_missing() {
        let store = MemoryStore::load(std::path::Path::new("/nonexistent/path/memory.json"))
            .expect("missing file should return empty store, not an error");
        assert!(store.notes.is_empty());
    }

    #[test]
    fn load_parses_existing_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let json =
            r#"{"notes":[{"id":"N1","tags":["rust"],"text":"hello","ts":"2026-01-01T00:00:00Z"}]}"#;
        std::fs::write(tmp.path(), json).unwrap();
        let store = MemoryStore::load(tmp.path()).expect("valid file must load");
        assert_eq!(store.notes.len(), 1);
        assert_eq!(store.notes[0].id, "N1");
        assert_eq!(store.notes[0].text, "hello");
    }

    // --- save + load round-trip ---

    #[test]
    fn save_and_reload_round_trip() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut store = fresh_store();
        store.add("round-trip text".into(), vec!["rt".into()]);
        store.save(tmp.path()).expect("save must succeed");

        let loaded = MemoryStore::load(tmp.path()).expect("reload must succeed");
        assert_eq!(loaded.notes.len(), 1);
        assert_eq!(loaded.notes[0].text, "round-trip text");
    }

    // --- next_id ---

    #[test]
    fn next_id_on_empty_store_is_n1() {
        assert_eq!(fresh_store().next_id(), "N1");
    }

    #[test]
    fn next_id_increments_past_existing_notes() {
        let mut store = fresh_store();
        store.add("first".into(), vec![]);
        store.add("second".into(), vec![]);
        assert_eq!(store.next_id(), "N3");
    }

    // --- add ---

    #[test]
    fn add_returns_the_assigned_id() {
        let mut store = fresh_store();
        let id = store.add("my note".into(), vec!["tag1".into()]);
        assert_eq!(id, "N1", "first add must return N1");
        assert_eq!(store.notes[0].id, "N1");
        assert_eq!(store.notes[0].text, "my note");
    }

    #[test]
    fn add_second_note_gets_n2() {
        let mut store = fresh_store();
        store.add("a".into(), vec![]);
        let id2 = store.add("b".into(), vec![]);
        assert_eq!(id2, "N2");
        assert_eq!(store.notes.len(), 2);
    }

    // --- remove ---

    #[test]
    fn remove_existing_note_returns_true() {
        let mut store = fresh_store();
        let id = store.add("to remove".into(), vec![]);
        assert!(store.remove(&id), "remove existing must return true");
        assert!(store.notes.is_empty(), "note must actually be gone");
    }

    #[test]
    fn remove_absent_note_returns_false() {
        let mut store = fresh_store();
        assert!(!store.remove("N999"), "remove absent must return false");
    }

    #[test]
    fn remove_only_removes_target_note() {
        let mut store = fresh_store();
        let id1 = store.add("keep".into(), vec![]);
        let id2 = store.add("remove me".into(), vec![]);
        assert!(store.remove(&id2));
        assert_eq!(store.notes.len(), 1);
        assert_eq!(store.notes[0].id, id1);
    }

    // --- search ---

    #[test]
    fn search_finds_by_text_substring() {
        let mut store = fresh_store();
        store.add("hello world".into(), vec![]);
        store.add("goodbye".into(), vec![]);
        let results = store.search(Some("hello"), None, 10);
        assert_eq!(results.len(), 1);
        assert!(results[0].text.contains("hello"));
    }

    #[test]
    fn search_finds_by_tag_in_text_or_tag_field() {
        let mut store = fresh_store();
        // Note with keyword only in the tags field (not text)
        store.add("general note".into(), vec!["special-tag".into()]);
        // Search by query that matches the tag string
        let results = store.search(Some("special-tag"), None, 10);
        assert_eq!(
            results.len(),
            1,
            "search must find query match in tags via || branch"
        );
    }

    #[test]
    fn search_exact_tag_filter() {
        let mut store = fresh_store();
        store.add("rust note".into(), vec!["rust".into()]);
        store.add("go note".into(), vec!["go".into()]);
        let results = store.search(None, Some("rust"), 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].tags[0], "rust");
    }

    #[test]
    fn search_limit_applies() {
        let mut store = fresh_store();
        for i in 0..5 {
            store.add(format!("note {i}"), vec![]);
        }
        let results = store.search(None, None, 3);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn search_no_match_returns_empty() {
        let mut store = fresh_store();
        store.add("hello".into(), vec![]);
        let results = store.search(Some("zzz_no_match"), None, 10);
        assert!(results.is_empty());
    }

    // --- chrono_now_rfc3339 ---

    #[test]
    fn chrono_now_rfc3339_is_nonempty_and_not_placeholder() {
        let ts = chrono_now_rfc3339();
        assert!(!ts.is_empty(), "timestamp must not be empty");
        assert_ne!(ts, "xyzzy", "timestamp must not be placeholder");
        // Must match RFC 3339 pattern: YYYY-MM-DDTHH:MM:SSZ
        assert!(
            ts.len() >= 20 && ts.ends_with('Z'),
            "timestamp must end with Z; got: {ts}"
        );
        assert!(
            ts.contains('T'),
            "timestamp must contain T separator; got: {ts}"
        );
    }

    #[test]
    fn chrono_now_rfc3339_year_is_plausible() {
        let ts = chrono_now_rfc3339();
        let year: u32 = ts[..4].parse().expect("first 4 chars must be a year");
        assert!(year >= 2024, "year must be >= 2024; got {year}");
    }

    // --- days_to_date ---

    #[test]
    fn days_to_date_day_zero_is_unix_epoch() {
        assert_eq!(days_to_date(0), (1970, 1, 1));
    }

    #[test]
    fn days_to_date_day_365_is_1971_01_01() {
        assert_eq!(days_to_date(365), (1971, 1, 1));
    }

    #[test]
    fn days_to_date_leap_year_day_60_is_feb_29() {
        // 1972 is a leap year. Day 365 (1971-01-01) + 365 (1971) = 730 → 1972-01-01.
        // Day 730 + 31 (Jan) + 29 (Feb leap) - 1 = 759 → 1972-02-29
        assert_eq!(days_to_date(730 + 59), (1972, 2, 29));
    }

    #[test]
    fn days_to_date_known_date_2026_07_06() {
        // 2026-07-06T00:00:00Z is exactly 20640 days since Unix epoch.
        let (y, m, d) = days_to_date(20640);
        assert_eq!(y, 2026);
        assert_eq!(m, 7);
        assert_eq!(d, 6);
    }

    // --- is_leap ---

    #[test]
    fn is_leap_regular_leap_year() {
        // divisible by 4, not 100 → leap
        assert!(is_leap(1972));
        assert!(is_leap(2024));
    }

    #[test]
    fn is_leap_century_non_leap() {
        // divisible by 100 but not 400 → NOT leap
        assert!(!is_leap(1900));
        assert!(!is_leap(1800));
    }

    #[test]
    fn is_leap_400_year_is_leap() {
        // divisible by 400 → leap
        assert!(is_leap(2000));
        assert!(is_leap(1600));
    }

    #[test]
    fn is_leap_ordinary_non_leap() {
        // not divisible by 4 → NOT leap
        assert!(!is_leap(1971));
        assert!(!is_leap(2023));
    }

    // --- chrono_now_rfc3339 arithmetic ---

    #[test]
    fn chrono_now_rfc3339_minutes_and_seconds_in_range() {
        let ts = chrono_now_rfc3339();
        // Format: YYYY-MM-DDTHH:MM:SSZ  (positions 14-15 = minutes, 17-18 = seconds)
        let mm: u32 = ts[14..16].parse().expect("minutes must parse");
        let ss: u32 = ts[17..19].parse().expect("seconds must parse");
        assert!(mm < 60, "minutes out of range: {mm}");
        assert!(ss < 60, "seconds out of range: {ss}");
    }

    // ── capacity / dedup / durable write ─────────────────────────────────────

    #[test]
    fn add_identical_text_refreshes_instead_of_duplicating() {
        let mut store = fresh_store();
        let id1 = store.add("same text".into(), vec!["a".into()]);
        let id2 = store.add("same text".into(), vec!["b".into()]);
        assert_eq!(id1, id2, "re-remembering the same text must reuse the id");
        assert_eq!(
            store.notes.len(),
            1,
            "duplicate text must not be stored twice"
        );
        assert_eq!(
            store.notes[0].tags,
            vec!["a".to_string(), "b".to_string()],
            "new tags must be merged into the existing note"
        );
    }

    #[test]
    fn add_refreshed_note_moves_to_the_newest_position() {
        // Re-confirming an old note must not leave it at the front of the
        // insertion order, where the next eviction would drop a note the user
        // just wrote.
        let mut store = fresh_store();
        store.add("oldest".into(), vec![]);
        store.add("newer".into(), vec![]);
        let refreshed = store.add("oldest".into(), vec![]);

        assert_eq!(refreshed, "N1");
        assert_eq!(store.notes.len(), 2);
        assert_eq!(store.notes[1].id, "N1", "refreshed note must be newest");

        let evicted = store.enforce_capacity(1);
        assert_eq!(evicted, vec!["N2".to_string()]);
        assert_eq!(store.notes[0].id, "N1");
    }

    #[test]
    fn add_different_text_still_appends() {
        let mut store = fresh_store();
        store.add("one".into(), vec![]);
        store.add("two".into(), vec![]);
        assert_eq!(store.notes.len(), 2, "distinct text must keep appending");
    }

    #[test]
    fn enforce_capacity_evicts_oldest_first() {
        let mut store = fresh_store();
        for i in 0..5 {
            store.add(format!("note {i}"), vec![]);
        }
        let evicted = store.enforce_capacity(3);
        assert_eq!(evicted, vec!["N1".to_string(), "N2".to_string()]);
        assert_eq!(store.notes.len(), 3);
        assert_eq!(store.notes[0].text, "note 2", "oldest notes go first");
    }

    #[test]
    fn enforce_capacity_below_cap_is_a_noop() {
        let mut store = fresh_store();
        store.add("only".into(), vec![]);
        assert!(store.enforce_capacity(1).is_empty());
        assert!(store.enforce_capacity(5).is_empty());
        assert_eq!(store.notes.len(), 1);
    }

    #[test]
    fn enforce_capacity_zero_disables_the_cap() {
        let mut store = fresh_store();
        for i in 0..3 {
            store.add(format!("n{i}"), vec![]);
        }
        assert!(store.enforce_capacity(0).is_empty());
        assert_eq!(store.notes.len(), 3, "0 means unlimited, not 'evict all'");
    }

    #[test]
    fn memory_max_notes_defaults_and_reads_env() {
        const VAR: &str = "RECURSIVE_MEMORY_MAX_NOTES";
        let _env_lock = crate::test_util::env_lock();
        let orig = std::env::var(VAR).ok();

        unsafe { std::env::remove_var(VAR) };
        assert_eq!(memory_max_notes(), 1000, "absent env var uses the default");

        unsafe { std::env::set_var(VAR, " 7 ") };
        assert_eq!(memory_max_notes(), 7, "surrounding whitespace is tolerated");

        unsafe { std::env::set_var(VAR, "not-a-number") };
        assert_eq!(
            memory_max_notes(),
            1000,
            "unparseable value falls back to the default"
        );

        unsafe {
            match orig {
                Some(v) => std::env::set_var(VAR, v),
                None => std::env::remove_var(VAR),
            }
        }
    }

    #[test]
    fn save_replaces_existing_content_and_leaves_no_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("memory.json");
        let mut store = fresh_store();
        store.add("first version".into(), vec![]);
        store.save(&path).unwrap();
        store.notes[0].text = "second version".into();
        store.save(&path).unwrap();

        let loaded = MemoryStore::load(&path).unwrap();
        assert_eq!(loaded.notes[0].text, "second version");

        let leftovers: Vec<String> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "atomic write must clean up its temp file: {leftovers:?}"
        );
    }

    #[tokio::test]
    async fn remember_evicts_oldest_notes_from_file_and_vector_store() {
        let tmp = tempfile::tempdir().unwrap();
        let vectors = Arc::new(NoopVectorStore::new());
        let tool = Remember::new(tmp.path())
            .with_vector_store(vectors.clone(), Arc::new(NoopEmbedding))
            .with_max_notes(2);

        for text in ["one", "two", "three"] {
            tool.execute(json!({ "text": text })).await.unwrap();
        }

        let stored = MemoryStore::load(&memory_path(tmp.path())).unwrap();
        assert_eq!(stored.notes.len(), 2, "cap must hold");
        assert_eq!(stored.notes[0].text, "two");
        assert_eq!(stored.notes[1].text, "three");

        let indexed = vectors.list_all().await.unwrap();
        assert_eq!(indexed.len(), 2, "evicted note must leave the vector index");
        assert!(
            !indexed.iter().any(|e| e.text == "one"),
            "the evicted note must not be recallable semantically"
        );
    }

    #[tokio::test]
    async fn forget_removes_the_vector_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let vectors = Arc::new(NoopVectorStore::new());
        Remember::new(tmp.path())
            .with_vector_store(vectors.clone(), Arc::new(NoopEmbedding))
            .execute(json!({ "text": "a secret" }))
            .await
            .unwrap();
        assert_eq!(vectors.list_all().await.unwrap().len(), 1);

        let out = Forget::new(tmp.path())
            .with_vector_store(vectors.clone())
            .execute(json!({ "id": "N1" }))
            .await
            .unwrap();

        assert_eq!(out, "removed N1");
        assert!(
            vectors.list_all().await.unwrap().is_empty(),
            "forget must delete the indexed copy too, or recall resurrects it"
        );
        assert!(MemoryStore::load(&memory_path(tmp.path()))
            .unwrap()
            .notes
            .is_empty());
    }

    #[tokio::test]
    async fn forget_unknown_id_reports_and_still_clears_the_index() {
        let tmp = tempfile::tempdir().unwrap();
        let vectors = Arc::new(NoopVectorStore::new());
        vectors
            .upsert(
                &MemoryEntry {
                    id: "N9".into(),
                    text: "dangling".into(),
                    tags: vec![],
                    ts: "2026-01-01T00:00:00Z".into(),
                },
                vec![],
            )
            .await
            .unwrap();

        let out = Forget::new(tmp.path())
            .with_vector_store(vectors.clone())
            .execute(json!({ "id": "N9" }))
            .await
            .unwrap();

        assert_eq!(out, "no such id: N9");
        assert!(
            vectors.list_all().await.unwrap().is_empty(),
            "the two stores can drift; forget must clear the index anyway"
        );
    }

    /// Counts `embed` calls so the empty-query guard in `Recall` is observable.
    struct CountingEmbedding {
        calls: Mutex<usize>,
    }

    #[async_trait]
    impl EmbeddingProvider for CountingEmbedding {
        async fn embed(&self, _text: &str) -> Vec<f32> {
            *self.calls.lock().unwrap_or_else(|e| e.into_inner()) += 1;
            vec![1.0, 0.0]
        }
    }

    #[tokio::test]
    async fn recall_does_not_embed_an_empty_query() {
        let tmp = tempfile::tempdir().unwrap();
        let path = memory_path(tmp.path());
        let mut store = MemoryStore::default();
        store.add("tagged note".into(), vec!["work".into()]);
        store.save(&path).unwrap();

        let embedding = Arc::new(CountingEmbedding {
            calls: Mutex::new(0),
        });
        let recall = Recall::new(tmp.path())
            .with_vector_store(Arc::new(NoopVectorStore::new()), embedding.clone());

        let by_tag = recall
            .execute(json!({ "tag": "work" }))
            .await
            .expect("tag-only recall");
        assert!(
            by_tag.contains("tagged note"),
            "tag filter must still work: {by_tag}"
        );
        assert_eq!(
            *embedding.calls.lock().unwrap_or_else(|e| e.into_inner()),
            0,
            "an empty query must not be sent to the embedding endpoint"
        );

        recall
            .execute(json!({ "query": "tagged" }))
            .await
            .expect("query recall");
        assert_eq!(
            *embedding.calls.lock().unwrap_or_else(|e| e.into_inner()),
            1,
            "a real query must still be embedded"
        );
    }

    #[tokio::test]
    async fn remember_indexes_the_tags_merged_into_an_existing_note() {
        let tmp = tempfile::tempdir().unwrap();
        let vectors = Arc::new(NoopVectorStore::new());
        let remember =
            Remember::new(tmp.path()).with_vector_store(vectors.clone(), Arc::new(NoopEmbedding));

        remember
            .execute(json!({ "text": "same note", "tags": ["a"] }))
            .await
            .unwrap();
        remember
            .execute(json!({ "text": "same note", "tags": ["b"] }))
            .await
            .unwrap();

        let indexed = vectors.list_all().await.unwrap();
        assert_eq!(indexed.len(), 1);
        assert_eq!(
            indexed[0].tags,
            vec!["a".to_string(), "b".to_string()],
            "the indexed copy must carry the tags merged into the file note"
        );

        let stored = MemoryStore::load(&memory_path(tmp.path())).unwrap();
        assert_eq!(
            stored.notes[0].tags, indexed[0].tags,
            "file and index must agree on a note's tags"
        );
    }

    #[tokio::test]
    async fn recall_applies_the_tag_filter_before_the_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let vectors = Arc::new(NoopVectorStore::new());
        for (id, text, tag) in [
            ("N1", "note one", "work"),
            ("N2", "note two", "other"),
            ("N3", "note three", "work"),
        ] {
            vectors
                .upsert(
                    &MemoryEntry {
                        id: id.into(),
                        text: text.into(),
                        tags: vec![tag.into()],
                        ts: "2026-01-01T00:00:00Z".into(),
                    },
                    vec![],
                )
                .await
                .unwrap();
        }

        let recall = Recall::new(tmp.path()).with_vector_store(
            vectors,
            Arc::new(CountingEmbedding {
                calls: Mutex::new(0),
            }),
        );

        let out = recall
            .execute(json!({ "query": "note", "tag": "work", "limit": 2 }))
            .await
            .unwrap();

        assert!(
            out.contains("note one") && out.contains("note three"),
            "both tagged notes must survive the limit: {out}"
        );
        assert!(
            !out.contains("note two"),
            "the untagged note must be filtered out: {out}"
        );
        assert_eq!(out.lines().count(), 2);
    }

    /// The production default wiring (feature off, or no embedding key) hands
    /// the *same* `NoopVectorStore` to every memory tool. It is in-process only,
    /// so it must never shadow the durable `memory.json`: a query that also
    /// matches a note written this session must still return the notes written
    /// in earlier sessions.
    #[tokio::test]
    async fn recall_still_sees_notes_from_earlier_sessions_in_the_default_build() {
        let tmp = tempfile::tempdir().unwrap();
        let path = memory_path(tmp.path());

        // A note from a previous session, on disk only — the in-process store
        // knows nothing about it.
        let mut prior = MemoryStore::default();
        prior.add("legacy widgets note".into(), vec![]);
        prior.save(&path).unwrap();

        let store = Arc::new(NoopVectorStore::new());
        let embedding = Arc::new(NoopEmbedding);
        Remember::new(tmp.path())
            .with_vector_store(store.clone(), embedding.clone())
            .execute(json!({ "text": "fresh widgets note" }))
            .await
            .unwrap();

        let out = Recall::new(tmp.path())
            .with_vector_store(store, embedding)
            .execute(json!({ "query": "widgets" }))
            .await
            .unwrap();

        assert!(
            out.contains("legacy widgets note"),
            "recall must still see notes written before this session: {out}"
        );
        assert!(out.contains("fresh widgets note"), "{out}");
    }

    /// `recall`'s documented contract is "most recent first". The shared no-op
    /// store used to answer from insertion order (oldest first) instead.
    #[tokio::test]
    async fn recall_is_most_recent_first_in_the_default_build() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(NoopVectorStore::new());
        let embedding = Arc::new(NoopEmbedding);
        for text in ["first widgets", "second widgets"] {
            Remember::new(tmp.path())
                .with_vector_store(store.clone(), embedding.clone())
                .execute(json!({ "text": text }))
                .await
                .unwrap();
        }

        let out = Recall::new(tmp.path())
            .with_vector_store(store, embedding)
            .execute(json!({ "query": "widgets" }))
            .await
            .unwrap();

        assert_eq!(
            out.lines().collect::<Vec<_>>(),
            vec!["N2 second widgets", "N1 first widgets"],
            "recall must be most recent first"
        );
    }

    /// With a real embedding the semantic hits lead, but they must be unioned
    /// with the durable file hits so a note the index has never seen is still
    /// returned.
    #[tokio::test]
    async fn recall_unions_semantic_hits_with_the_durable_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = memory_path(tmp.path());

        // Only N1 is present in the vector index; N2 lives only in memory.json.
        let store = Arc::new(NoopVectorStore::new());
        store
            .upsert(
                &MemoryEntry {
                    id: "N1".into(),
                    text: "indexed widgets".into(),
                    tags: vec![],
                    ts: "2026-01-01T00:00:00Z".into(),
                },
                vec![],
            )
            .await
            .unwrap();
        let mut file = MemoryStore::default();
        file.add("indexed widgets".into(), vec![]);
        file.add("unindexed widgets".into(), vec![]);
        file.save(&path).unwrap();

        let recall = Recall::new(tmp.path()).with_vector_store(
            store,
            Arc::new(CountingEmbedding {
                calls: Mutex::new(0),
            }),
        );
        let out = recall.execute(json!({ "query": "widgets" })).await.unwrap();

        assert!(
            out.contains("unindexed widgets"),
            "a pre-index note must not become unreachable: {out}"
        );
        assert!(out.contains("indexed widgets"), "{out}");
    }

    // ── Scratchpad unit tests ────────────────────────────────────────────────

    fn fresh_scratchpad() -> Scratchpad {
        Scratchpad::default()
    }

    #[test]
    fn scratchpad_set_inserts_new_entry() {
        // kills function-level replacement of Scratchpad::set
        let mut pad = fresh_scratchpad();
        pad.set("k1".into(), "v1".into());
        assert_eq!(pad.get("k1"), Some("v1"));
    }

    #[test]
    fn scratchpad_set_updates_existing_entry() {
        // kills the `if let Some(existing)` branch: without update, old value persists
        let mut pad = fresh_scratchpad();
        pad.set("key".into(), "first".into());
        pad.set("key".into(), "second".into());
        assert_eq!(
            pad.get("key"),
            Some("second"),
            "set must update existing key"
        );
        assert_eq!(
            pad.entries.len(),
            1,
            "update must not add a duplicate entry"
        );
    }

    #[test]
    fn scratchpad_get_returns_none_for_missing_key() {
        // kills `_ => Some(...)` mutations in get
        let pad = fresh_scratchpad();
        assert!(pad.get("nonexistent").is_none());
    }

    #[test]
    fn scratchpad_delete_returns_true_for_existing_key() {
        // kills `entries.len() < before` mutations and function-level replacement
        let mut pad = fresh_scratchpad();
        pad.set("k".into(), "v".into());
        assert!(pad.delete("k"), "delete must return true when key existed");
        assert!(pad.get("k").is_none(), "key must be gone after delete");
    }

    #[test]
    fn scratchpad_delete_returns_false_for_missing_key() {
        // kills `!= k` → `== k` retain mutation
        let mut pad = fresh_scratchpad();
        assert!(
            !pad.delete("ghost"),
            "delete must return false for absent key"
        );
    }

    #[test]
    fn scratchpad_keys_returns_all_keys_in_order() {
        // kills function-level replacement of Scratchpad::keys
        let mut pad = fresh_scratchpad();
        pad.set("alpha".into(), "1".into());
        pad.set("beta".into(), "2".into());
        let keys = pad.keys();
        assert_eq!(keys, vec!["alpha", "beta"]);
    }

    #[test]
    fn scratchpad_summary_returns_empty_for_no_entries() {
        // kills `if pad.entries.is_empty()` guard removal
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let summary = scratchpad_summary(tmp.path());
        assert!(
            summary.is_empty(),
            "empty scratchpad must produce empty summary, got: {summary}"
        );
    }

    #[test]
    fn scratchpad_summary_truncates_long_values() {
        // kills `if entry.value.len() > 200` guard removal / off-by-one mutations
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let path = scratchpad_path(tmp.path());
        let mut pad = Scratchpad::default();
        pad.set("long_key".into(), "X".repeat(300));
        pad.save(&path).unwrap();

        let summary = scratchpad_summary(tmp.path());
        assert!(
            !summary.contains(&"X".repeat(300)),
            "value > 200 chars must be truncated in summary"
        );
        assert!(
            summary.contains("..."),
            "truncated value must end with '...': {summary}"
        );
    }

    // ── memory_summary unit tests ───────────────────────────────────────────────

    #[test]
    fn memory_summary_returns_empty_for_no_notes() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let summary = memory_summary(tmp.path(), 10);
        assert!(
            summary.is_empty(),
            "empty memory must produce empty summary, got: {summary}"
        );
    }

    #[test]
    fn memory_summary_uses_index_format() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let path = memory_path(tmp.path());
        let mut store = MemoryStore::default();
        store.add("test note content".into(), vec!["tag1".into()]);
        store.save(&path).unwrap();

        let summary = memory_summary(tmp.path(), 10);
        assert!(
            summary.contains("# Memory Index"),
            "summary must start with '# Memory Index'"
        );
        assert!(
            summary.contains("- [N1](recall) — test note content"),
            "summary must use index format: '- [ID](recall) — hook'"
        );
        assert!(
            !summary.contains("[tag1]"),
            "index format should not inline tags (they're in the full note)"
        );
    }

    #[test]
    fn memory_summary_truncates_long_hooks() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let path = memory_path(tmp.path());
        let mut store = MemoryStore::default();
        store.add("X".repeat(200), vec![]);
        store.save(&path).unwrap();

        let summary = memory_summary(tmp.path(), 10);
        assert!(
            summary.contains("..."),
            "long hook must be truncated with '...'"
        );
        // Verify the index format is preserved even with truncation
        assert!(
            summary.contains("- [N1](recall) — "),
            "truncated hook must still use index format"
        );
    }

    #[test]
    fn memory_summary_replaces_newlines_in_hook() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let path = memory_path(tmp.path());
        let mut store = MemoryStore::default();
        store.add("line1\nline2\nline3".into(), vec![]);
        store.save(&path).unwrap();

        let summary = memory_summary(tmp.path(), 10);
        // Find the index line (starts with "- [N")
        let index_line = summary
            .lines()
            .find(|l| l.starts_with("- [N"))
            .expect("should find an index line");
        assert!(
            !index_line.contains('\n'),
            "newlines in hook must be replaced with spaces to keep index compact"
        );
        assert!(
            index_line.contains("line1 line2 line3") || index_line.contains("line1 line2"),
            "hook content should be preserved with spaces instead of newlines"
        );
    }

    #[test]
    fn memory_summary_respects_limit() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let path = memory_path(tmp.path());
        let mut store = MemoryStore::default();
        for i in 0..10 {
            store.add(format!("note {}", i), vec![]);
        }
        store.save(&path).unwrap();

        let summary = memory_summary(tmp.path(), 3);
        // Should have header + 3 entries
        let entry_count = summary.matches("- [N").count();
        assert_eq!(entry_count, 3, "summary must respect limit parameter");
    }

    #[test]
    fn memory_summary_most_recent_first() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let path = memory_path(tmp.path());
        let mut store = MemoryStore::default();
        store.add("first".into(), vec![]);
        store.add("second".into(), vec![]);
        store.add("third".into(), vec![]);
        store.save(&path).unwrap();

        let summary = memory_summary(tmp.path(), 10);
        let lines: Vec<&str> = summary.lines().collect();
        // Find the index lines (starting with "- [N")
        let mut index_lines = Vec::new();
        for line in lines {
            if line.starts_with("- [N") {
                index_lines.push(line);
            }
        }
        // Most recent (N3, N2, N1) should appear first
        assert!(
            index_lines[0].contains("N3"),
            "most recent note (N3) should appear first"
        );
        assert!(
            index_lines[2].contains("N1"),
            "oldest note (N1) should appear last"
        );
    }
}
