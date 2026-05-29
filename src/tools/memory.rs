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
use std::sync::Mutex;

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
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
            message: format!("failed to read memory file: {e}"),
        })?;
        serde_json::from_str(&raw).map_err(|e| Error::Tool {
            name: "memory".into(),
            message: format!("malformed memory file: {e}"),
        })
    }

    /// Save to disk, creating parent directories if needed.
    fn save(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Tool {
                name: "memory".into(),
                message: format!("failed to create memory directory: {e}"),
            })?;
        }
        let raw = serde_json::to_string_pretty(self).map_err(|e| Error::Tool {
            name: "memory".into(),
            message: format!("failed to serialize memory: {e}"),
        })?;
        std::fs::write(path, raw).map_err(|e| Error::Tool {
            name: "memory".into(),
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
}

impl Remember {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            lock: Mutex::new(()),
        }
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

    fn is_readonly(&self) -> bool {
        true
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

        let _guard = self.lock.lock().unwrap();
        let path = memory_path(&self.workspace);
        let mut store = MemoryStore::load(&path)?;
        let id = store.add(text, tags);
        store.save(&path)?;
        Ok(format!("saved note {id}"))
    }
}

pub struct Recall {
    workspace: PathBuf,
}

impl Recall {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
        }
    }
}

#[async_trait]
impl Tool for Recall {
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
        let query = arguments["query"].as_str();
        let tag = arguments["tag"].as_str();
        let limit = arguments["limit"].as_i64().unwrap_or(10) as usize;

        let path = memory_path(&self.workspace);
        let store = MemoryStore::load(&path)?;
        let results = store.search(query, tag, limit);

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

        let _guard = self.lock.lock().unwrap();
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
            message: format!("failed to read scratchpad file: {e}"),
        })?;
        serde_json::from_str(&raw).map_err(|e| Error::Tool {
            name: "scratchpad".into(),
            message: format!("malformed scratchpad file: {e}"),
        })
    }

    /// Save to disk, creating parent directories if needed.
    fn save(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Tool {
                name: "scratchpad".into(),
                message: format!("failed to create scratchpad directory: {e}"),
            })?;
        }
        let raw = serde_json::to_string_pretty(self).map_err(|e| Error::Tool {
            name: "scratchpad".into(),
            message: format!("failed to serialize scratchpad: {e}"),
        })?;
        std::fs::write(path, raw).map_err(|e| Error::Tool {
            name: "scratchpad".into(),
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

        let _guard = self.lock.lock().unwrap();
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

        let _guard = self.lock.lock().unwrap();
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
