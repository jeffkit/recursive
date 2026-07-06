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

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
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
        std::fs::write(path, raw).map_err(|e| Error::Tool {
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
    fn add(&mut self, text: String, tags: Vec<String>) -> String {
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

/// Load the memory store from the workspace-relative path.
pub fn load_memory(workspace: &std::path::Path) -> Result<MemoryStore> {
    let path = memory_path(workspace);
    MemoryStore::load(&path)
}

/// Build a memory summary string for injection into the system prompt.
/// Returns the top N most recent notes as a formatted block, or empty
/// string if no notes exist.
pub fn memory_summary(workspace: &std::path::Path, limit: usize) -> String {
    let store = match load_memory(workspace) {
        Ok(s) => s,
        Err(_) => return String::new(),
    };
    if store.notes.is_empty() {
        return String::new();
    }
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "# Memory (top {} most recent notes; use `recall` for more)",
        limit
    ));
    // Most recent first
    let mut notes: Vec<&Note> = store.notes.iter().collect();
    notes.reverse();
    for note in notes.iter().take(limit) {
        let tags_str = if note.tags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", note.tags.join(","))
        };
        // Truncate long text for the summary
        let text_preview = if note.text.len() > 120 {
            format!("{}...", crate::truncate_str(&note.text, 117))
        } else {
            note.text.clone()
        };
        lines.push(format!("- {}{} {}", note.id, tags_str, text_preview));
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

pub struct Remember {
    workspace: PathBuf,
    /// Mutex for thread-safe access to the memory file.
    lock: Mutex<()>,
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
            vector_store: Arc::new(NoopVectorStore::new()),
            embedding_provider: Arc::new(NoopEmbedding),
        }
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
        let id = file_store.add(text.clone(), tags.clone());
        file_store.save(&path)?;
        drop(_guard);

        // Also index in the vector store if one is configured.
        let ts = chrono::Utc::now().to_rfc3339();
        let entry = MemoryEntry {
            id: id.clone(),
            text: text.clone(),
            tags,
            ts,
        };
        let vector = self.embedding_provider.embed(&text).await;
        if let Err(e) = self.vector_store.upsert(&entry, vector).await {
            tracing::warn!(error = %e, note_id = %id, "remember: vector upsert failed");
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

        // Try vector search first; fall back to file-based keyword search
        // when the vector store or embedding provider is a no-op.
        let query_vec = self.embedding_provider.embed(query).await;
        let use_vector = !query_vec.is_empty();

        if use_vector || tag.is_none() {
            // Use the vector store (semantic or keyword fallback inside the store).
            match self.vector_store.search(query_vec, query, limit).await {
                Ok(entries) if !entries.is_empty() => {
                    // Apply tag filter if requested.
                    let filtered: Vec<_> = entries
                        .iter()
                        .filter(|e| tag.map_or(true, |t| e.tags.iter().any(|et| et == t)))
                        .take(limit)
                        .collect();
                    if !filtered.is_empty() {
                        let lines: Vec<String> = filtered
                            .iter()
                            .map(|e| {
                                let tags_str = if e.tags.is_empty() {
                                    String::new()
                                } else {
                                    format!(" [{}]", e.tags.join(","))
                                };
                                format!("{}{} {}", e.id, tags_str, e.text)
                            })
                            .collect();
                        return Ok(lines.join("\n"));
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "recall: vector search failed, falling back to file");
                }
            }
        }

        // File-based fallback (or when only tag filter is used).
        let path = memory_path(&self.workspace);
        let file_store = MemoryStore::load(&path)?;
        let query_opt = if query.is_empty() { None } else { Some(query) };
        let results = file_store.search(query_opt, tag, limit);

        if results.is_empty() {
            return Ok("no matching notes found".to_string());
        }

        let lines: Vec<String> = results
            .iter()
            .map(|n| {
                let tags_str = if n.tags.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", n.tags.join(","))
                };
                format!("{}{} {}", n.id, tags_str, n.text)
            })
            .collect();

        Ok(lines.join("\n"))
    }
}

pub struct Forget {
    workspace: PathBuf,
    lock: Mutex<()>,
}

impl Forget {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            lock: Mutex::new(()),
        }
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
        if store.remove(&id) {
            store.save(&path)?;
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
        let json = r#"{"notes":[{"id":"N1","tags":["rust"],"text":"hello","ts":"2026-01-01T00:00:00Z"}]}"#;
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
        assert_eq!(results.len(), 1, "search must find query match in tags via || branch");
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
        assert!(ts.contains('T'), "timestamp must contain T separator; got: {ts}");
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
        assert_eq!(pad.get("key"), Some("second"), "set must update existing key");
        assert_eq!(pad.entries.len(), 1, "update must not add a duplicate entry");
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
        assert!(!pad.delete("ghost"), "delete must return false for absent key");
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
}
