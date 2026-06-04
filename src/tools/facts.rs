//! Memory Layer 2 — Semantic facts with search.
//!
//! Extends the existing `remember`/`recall`/`forget` tools with structured
//! facts storage, pure Rust full-text search, deduplication, and eviction.
//!
//! Facts are stored in JSONL format at:
//!   <workspace>/.recursive/memory/facts.jsonl  (workspace scope)
//!   ~/.recursive/memory/facts.jsonl            (global scope)
//!
//! Each fact has:
//! - `id` (monotonic "F1", "F2", ...)
//! - `text` (the fact content)
//! - `tags` (optional categorisation)
//! - `source` (optional provenance, e.g. "user", "agent")
//! - `created_at` (RFC 3339 timestamp)
//! - `last_accessed` (RFC 3339 timestamp, updated on recall)
//! - `access_count` (how many times recalled)
//! - `superseded_by` (optional fact ID that replaces this one — soft delete)

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::tools::Tool;

// ---------------------------------------------------------------------------
// Fact data structures
// ---------------------------------------------------------------------------

/// A single semantic fact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fact {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub source: Option<String>,
    pub created_at: String,
    pub last_accessed: String,
    pub access_count: u64,
    #[serde(default)]
    pub superseded_by: Option<String>,
}

impl Fact {
    /// Whether this fact is considered "active" (not superseded).
    fn is_active(&self) -> bool {
        self.superseded_by.is_none()
    }
}

/// The on-disk fact store (JSONL).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FactStore {
    pub facts: Vec<Fact>,
}

impl FactStore {
    /// Load facts from a JSONL file path. Returns empty store if file doesn't exist.
    fn load(path: &std::path::Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path).map_err(|e| Error::Tool {
            name: "facts".into(),
            message: format!("failed to read facts file: {e}"),
        })?;
        let mut facts = Vec::new();
        for (i, line) in raw.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<Fact>(line) {
                Ok(fact) => facts.push(fact),
                Err(e) => {
                    return Err(Error::Tool {
                        name: "facts".into(),
                        message: format!("malformed fact at line {}: {e}", i + 1),
                    });
                }
            }
        }
        Ok(Self { facts })
    }

    /// Save facts to a JSONL file, creating parent directories if needed.
    fn save(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Tool {
                name: "facts".into(),
                message: format!("failed to create facts directory: {e}"),
            })?;
        }
        let mut out = String::new();
        for fact in &self.facts {
            out.push_str(&serde_json::to_string(fact).map_err(|e| Error::Tool {
                name: "facts".into(),
                message: format!("failed to serialize fact: {e}"),
            })?);
            out.push('\n');
        }
        std::fs::write(path, out).map_err(|e| Error::Tool {
            name: "facts".into(),
            message: format!("failed to write facts file: {e}"),
        })?;
        Ok(())
    }

    /// Generate the next monotonic fact ID.
    fn next_id(&self) -> String {
        let max = self
            .facts
            .iter()
            .filter_map(|f| f.id.strip_prefix('F'))
            .filter_map(|s| s.parse::<u32>().ok())
            .max()
            .unwrap_or(0);
        format!("F{}", max + 1)
    }

    /// Add a new fact, returning its ID.
    fn add(&mut self, text: String, tags: Vec<String>, source: Option<String>) -> String {
        let id = self.next_id();
        let now = chrono_now_rfc3339();
        self.facts.push(Fact {
            id: id.clone(),
            text,
            tags,
            source,
            created_at: now.clone(),
            last_accessed: now,
            access_count: 0,
            superseded_by: None,
        });
        id
    }

    /// Find a fact by ID.
    fn get(&self, id: &str) -> Option<&Fact> {
        self.facts.iter().find(|f| f.id == id)
    }

    /// Get a mutable reference to a fact by ID.
    fn get_mut(&mut self, id: &str) -> Option<&mut Fact> {
        self.facts.iter_mut().find(|f| f.id == id)
    }

    /// Soft-delete a fact by setting `superseded_by`.
    fn soft_delete(&mut self, id: &str, superseded_by: &str) -> bool {
        if let Some(fact) = self.get_mut(id) {
            fact.superseded_by = Some(superseded_by.to_string());
            true
        } else {
            false
        }
    }

    /// Return all active (non-superseded) facts.
    fn active_facts(&self) -> Vec<&Fact> {
        self.facts.iter().filter(|f| f.is_active()).collect()
    }

    /// Evict the stalest facts until we're under `cap` active facts.
    /// Staleness = days since last_accessed * (1.0 / (access_count + 1)).
    /// Returns the number of facts evicted.
    fn evict_to_cap(&mut self, cap: usize) -> usize {
        let active_count = self.active_facts().len();
        if active_count <= cap {
            return 0;
        }
        let to_remove = active_count - cap;

        // Score each active fact by staleness
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        let mut scored: Vec<(usize, f64)> = self
            .facts
            .iter()
            .enumerate()
            .filter(|(_, f)| f.is_active())
            .map(|(i, f)| {
                let last_access_secs = rfc3339_to_secs(&f.last_accessed).unwrap_or(0.0);
                let days_since = (now_secs - last_access_secs) / 86400.0;
                let staleness = days_since * (1.0 / (f.access_count.max(1) as f64));
                (i, staleness)
            })
            .collect();
        // Sort by staleness descending (most stale first)
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let mut evicted = 0;
        for (idx, _) in scored.iter().take(to_remove) {
            // Mark as superseded by a sentinel
            self.facts[*idx].superseded_by = Some("__evicted__".to_string());
            evicted += 1;
        }
        evicted
    }
}

// ---------------------------------------------------------------------------
// Full-text search (pure Rust)
// ---------------------------------------------------------------------------

/// A search result with a relevance score.
#[derive(Debug, Clone)]
pub struct ScoredFact {
    pub fact: Fact,
    pub score: f64,
}

/// Tokenize text into lowercase terms, splitting on whitespace and punctuation.
fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| c.is_whitespace() || c.is_ascii_punctuation())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect()
}

/// Stop words to filter out.
const STOP_WORDS: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "in", "on", "at", "to", "for", "of", "with", "by",
    "from", "is", "are", "was", "were", "be", "been", "being", "have", "has", "had", "do", "does",
    "did", "will", "would", "could", "should", "may", "might", "shall", "can", "it", "its", "this",
    "that", "these", "those", "i", "you", "he", "she", "we", "they", "not", "no", "nor", "so",
    "if", "then", "than", "too", "very", "just", "about", "up", "out", "also", "more", "some",
    "any", "each", "every", "all", "both", "few", "most", "other", "into", "over", "such", "only",
    "own", "same", "as", "but", "not",
];

/// Search facts by query text, returning scored results sorted by relevance.
///
/// Scoring formula:
///   score = term_frequency * tag_match * recency_boost * popularity_boost
///
/// - term_frequency: fraction of query terms that appear in the fact text
/// - tag_match: 1.2 if any query term matches a tag, else 1.0
/// - recency_boost: 1.0 + 0.1 / (days_since_created + 1)
/// - popularity_boost: 1.0 + 0.05 * ln(access_count + 1)
pub fn search_facts(
    facts: &[&Fact],
    query: &str,
    tag: Option<&str>,
    limit: usize,
) -> Vec<ScoredFact> {
    let query_terms: Vec<String> = tokenize(query)
        .into_iter()
        .filter(|t| !STOP_WORDS.contains(&t.as_str()))
        .collect();

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();

    let mut scored: Vec<ScoredFact> = facts
        .iter()
        .filter(|f| f.is_active())
        .filter(|f| {
            // Tag filter
            tag.map_or(true, |t| f.tags.iter().any(|ft| ft == t))
        })
        .filter(|f| {
            // If no query terms, include all (tag-only search)
            if query_terms.is_empty() {
                return true;
            }
            // At least one query term must appear in text or tags
            let text_lower = f.text.to_lowercase();
            let tag_text: String = f.tags.join(" ").to_lowercase();
            query_terms
                .iter()
                .any(|t| text_lower.contains(t) || tag_text.contains(t))
        })
        .map(|f| {
            let text_lower = f.text.to_lowercase();
            let tag_text: String = f.tags.join(" ").to_lowercase();

            // Term frequency: fraction of query terms present
            let term_frequency = if query_terms.is_empty() {
                1.0
            } else {
                let present = query_terms
                    .iter()
                    .filter(|t| text_lower.contains(t.as_str()) || tag_text.contains(t.as_str()))
                    .count();
                present as f64 / query_terms.len() as f64
            };

            // Tag match boost
            let tag_match = if query_terms.iter().any(|t| tag_text.contains(t)) {
                1.2
            } else {
                1.0
            };

            // Recency boost
            let created_secs = rfc3339_to_secs(&f.created_at).unwrap_or(0.0);
            let days_since = ((now_secs - created_secs) / 86400.0).max(0.0);
            let recency_boost = 1.0 + 0.1 / (days_since + 1.0);

            // Popularity boost
            let popularity_boost = 1.0 + 0.05 * ((f.access_count + 1) as f64).ln();

            let score = term_frequency * tag_match * recency_boost * popularity_boost;

            ScoredFact {
                fact: (*f).clone(),
                score,
            }
        })
        .collect();

    // Sort by score descending
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(limit);
    scored
}

// ---------------------------------------------------------------------------
// Deduplication (Jaccard similarity)
// ---------------------------------------------------------------------------

/// Compute Jaccard similarity between two strings (token-level).
fn jaccard_similarity(a: &str, b: &str) -> f64 {
    let tokens_a: HashSet<String> = tokenize(a).into_iter().collect();
    let tokens_b: HashSet<String> = tokenize(b).into_iter().collect();

    if tokens_a.is_empty() && tokens_b.is_empty() {
        return 1.0;
    }

    let intersection: HashSet<&String> = tokens_a.intersection(&tokens_b).collect();
    let union_size = tokens_a.len() + tokens_b.len() - intersection.len();
    if union_size == 0 {
        return 0.0;
    }
    intersection.len() as f64 / union_size as f64
}

/// Result of a duplicate-fact check.
enum DuplicateResult {
    /// The new text is longer/more specific — supersede the existing fact with this ID.
    SupersedeExisting(String),
    /// The existing fact is at least as specific — keep it, discard the new text.
    KeepExisting(String),
}

/// Check if a new fact text is a duplicate of an existing active fact.
/// Returns `Some(DuplicateResult)` if similarity >= threshold, `None` otherwise.
fn find_duplicate(facts: &[&Fact], text: &str, threshold: f64) -> Option<DuplicateResult> {
    for fact in facts {
        if !fact.is_active() {
            continue;
        }
        let sim = jaccard_similarity(&fact.text, text);
        if sim >= threshold {
            if text.len() > fact.text.len() {
                return Some(DuplicateResult::SupersedeExisting(fact.id.clone()));
            }
            return Some(DuplicateResult::KeepExisting(fact.id.clone()));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Timestamp helpers
// ---------------------------------------------------------------------------

/// Get an RFC 3339 timestamp string.
fn chrono_now_rfc3339() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;

    let (year, month, day) = days_to_date(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hours, minutes, seconds
    )
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_date(mut days: u64) -> (u64, u64, u64) {
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
    let day = days + 1;
    (year, month, day)
}

fn is_leap(year: u64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Parse an RFC 3339 timestamp to seconds since Unix epoch.
fn rfc3339_to_secs(s: &str) -> Option<f64> {
    // Format: "2026-05-25T12:34:56Z"
    if s.len() < 20 {
        return None;
    }
    let year: u64 = s[0..4].parse().ok()?;
    let month: u64 = s[5..7].parse().ok()?;
    let day: u64 = s[8..10].parse().ok()?;
    let hour: u64 = s[11..13].parse().ok()?;
    let min: u64 = s[14..16].parse().ok()?;
    let sec: u64 = s[17..19].parse().ok()?;

    let days = days_since_epoch(year, month, day);
    Some((days * 86400 + hour * 3600 + min * 60 + sec) as f64)
}

fn days_since_epoch(year: u64, month: u64, day: u64) -> u64 {
    let mut total = 0u64;
    for yr in 1970..year {
        total += if is_leap(yr) { 366 } else { 365 };
    }
    let months_days: [u64; 12] = if is_leap(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    for md in months_days.iter().take((month - 1) as usize) {
        total += md;
    }
    total + day - 1
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Determine the facts file path for a given scope.
pub fn facts_path(workspace: &std::path::Path, scope: &str) -> PathBuf {
    match scope {
        "global" => {
            if let Some(home) = std::env::var_os("HOME") {
                PathBuf::from(home)
                    .join(".recursive")
                    .join("memory")
                    .join("facts.jsonl")
            } else {
                workspace
                    .join(".recursive")
                    .join("memory")
                    .join("facts.jsonl")
            }
        }
        _ => workspace
            .join(".recursive")
            .join("memory")
            .join("facts.jsonl"),
    }
}

/// Load the fact store for a given scope.
pub fn load_facts(workspace: &std::path::Path, scope: &str) -> Result<FactStore> {
    let path = facts_path(workspace, scope);
    FactStore::load(&path)
}

/// Build a facts summary string for injection into the system prompt.
/// Merges workspace-scoped and global-scoped facts, then returns the top N
/// most recently accessed facts as a formatted block.
pub fn facts_summary(workspace: &std::path::Path, limit: usize) -> String {
    // Load both scopes; silently ignore missing files.
    let workspace_store = load_facts(workspace, "workspace").unwrap_or_default();
    let global_store = load_facts(workspace, "global").unwrap_or_default();

    // Merge active facts from both scopes; global facts come first so that
    // user-identity facts survive even when workspace has many project facts.
    let mut all_facts: Vec<Fact> = global_store
        .active_facts()
        .into_iter()
        .chain(workspace_store.active_facts())
        .cloned()
        .collect();

    if all_facts.is_empty() {
        return String::new();
    }

    // Sort by last_accessed descending (most recently accessed first)
    all_facts.sort_by(|a, b| b.last_accessed.cmp(&a.last_accessed));
    let sorted: Vec<&Fact> = all_facts.iter().collect();

    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "# Facts (top {} most recently accessed; use `recall` for more)",
        limit
    ));
    for fact in sorted.iter().take(limit) {
        let tags_str = if fact.tags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", fact.tags.join(","))
        };
        let source_str = if let Some(ref src) = fact.source {
            format!(" (source: {})", src)
        } else {
            String::new()
        };
        let text_preview = if fact.text.chars().count() > 120 {
            format!("{}...", crate::truncate_str(&fact.text, 117))
        } else {
            fact.text.clone()
        };
        lines.push(format!(
            "- {}{}{} {}",
            fact.id, tags_str, source_str, text_preview
        ));
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

/// Maximum active facts per scope before eviction kicks in.
const FACTS_CAP: usize = 500;

/// Jaccard similarity threshold for deduplication.
const DEDUP_THRESHOLD: f64 = 0.7;

pub struct RememberFact {
    workspace: PathBuf,
    lock: Mutex<()>,
}

impl RememberFact {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            lock: Mutex::new(()),
        }
    }
}

#[async_trait]
impl Tool for RememberFact {
    fn is_deferred(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "remember".into(),
            description: "Save a fact to persistent memory. The fact will be available in future sessions via `recall` or injected into the system prompt. Supports deduplication (similar facts are merged) and scoping (workspace vs global).".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "The fact text to remember"
                    },
                    "tags": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Optional tags for categorising the fact"
                    },
                    "source": {
                        "type": "string",
                        "description": "Optional source/provenance of the fact (e.g. 'user', 'agent')"
                    },
                    "scope": {
                        "type": "string",
                        "description": "Scope: 'workspace' (default) or 'global'",
                        "default": "workspace"
                    }
                },
                "required": ["text"]
            }),
        }
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

        let source: Option<String> = arguments["source"].as_str().map(String::from);

        let scope = arguments["scope"]
            .as_str()
            .unwrap_or("workspace")
            .to_string();

        let _guard = self.lock.lock().unwrap();
        let path = facts_path(&self.workspace, &scope);
        let mut store = FactStore::load(&path)?;

        // Deduplication check
        let active: Vec<&Fact> = store.active_facts();
        match find_duplicate(&active, &text, DEDUP_THRESHOLD) {
            Some(DuplicateResult::SupersedeExisting(dup_id)) => {
                store.soft_delete(&dup_id, &store.next_id());
                let id = store.add(text, tags, source);
                store.save(&path)?;
                return Ok(format!("saved fact {id} (superseded {dup_id})"));
            }
            Some(DuplicateResult::KeepExisting(dup_id)) => {
                if let Some(existing) = store.get_mut(&dup_id) {
                    existing.last_accessed = chrono_now_rfc3339();
                    existing.access_count += 1;
                }
                store.save(&path)?;
                return Ok(format!("duplicate of {dup_id}, kept existing"));
            }
            None => {}
        }

        // Eviction check
        store.evict_to_cap(FACTS_CAP);
        // Evict to cap-1 so that after adding the new fact we stay at cap
        store.evict_to_cap(FACTS_CAP.saturating_sub(1));

        let id = store.add(text, tags, source);
        store.save(&path)?;
        Ok(format!("saved fact {id}"))
    }
}

pub struct RecallFact {
    workspace: PathBuf,
}

impl RecallFact {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
        }
    }
}

#[async_trait]
impl Tool for RecallFact {
    fn is_deferred(&self) -> bool {
        true
    }

    fn side_effect_class(&self) -> crate::tools::ToolSideEffect {
        crate::tools::ToolSideEffect::ReadOnly
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "recall".into(),
            description: "Search persistent memory for facts matching a query or tag. Returns up to `limit` results, sorted by relevance. Also supports scoping (workspace vs global).".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query for full-text search across fact text and tags"
                    },
                    "tag": {
                        "type": "string",
                        "description": "Exact tag to filter by"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of results (default 10)",
                        "default": 10
                    },
                    "scope": {
                        "type": "string",
                        "description": "Scope: 'workspace' (default) or 'global'",
                        "default": "workspace"
                    }
                }
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let query = arguments["query"].as_str().unwrap_or("");
        let tag = arguments["tag"].as_str();
        let limit = arguments["limit"].as_i64().unwrap_or(10) as usize;
        let scope = arguments["scope"].as_str().unwrap_or("workspace");

        let path = facts_path(&self.workspace, scope);
        let mut store = FactStore::load(&path)?;
        let active: Vec<&Fact> = store.active_facts();

        let results = if query.is_empty() && tag.is_none() {
            // No query: return most recently accessed
            let mut sorted: Vec<&Fact> = active;
            sorted.sort_by(|a, b| b.last_accessed.cmp(&a.last_accessed));
            sorted.truncate(limit);
            sorted
                .into_iter()
                .map(|f| ScoredFact {
                    fact: f.clone(),
                    score: 0.0,
                })
                .collect()
        } else {
            search_facts(&active, query, tag, limit)
        };

        if results.is_empty() {
            return Ok("no matching facts found".to_string());
        }

        // Update access stats for returned facts
        for scored in &results {
            if let Some(fact) = store.get_mut(&scored.fact.id) {
                fact.last_accessed = chrono_now_rfc3339();
                fact.access_count += 1;
            }
        }
        store.save(&path)?;

        let lines: Vec<String> = results
            .iter()
            .map(|sf| {
                let tags_str = if sf.fact.tags.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", sf.fact.tags.join(","))
                };
                let source_str = if let Some(ref src) = sf.fact.source {
                    format!(" (source: {})", src)
                } else {
                    String::new()
                };
                format!("{}{}{} {}", sf.fact.id, tags_str, source_str, sf.fact.text)
            })
            .collect();

        Ok(lines.join("\n"))
    }
}

pub struct ForgetFact {
    workspace: PathBuf,
    lock: Mutex<()>,
}

impl ForgetFact {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            lock: Mutex::new(()),
        }
    }
}

#[async_trait]
impl Tool for ForgetFact {
    fn is_deferred(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "forget".into(),
            description: "Remove a fact from persistent memory by its ID (soft delete).".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "The ID of the fact to remove (e.g. F3)"
                    },
                    "scope": {
                        "type": "string",
                        "description": "Scope: 'workspace' (default) or 'global'",
                        "default": "workspace"
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

        let scope = arguments["scope"].as_str().unwrap_or("workspace");

        let _guard = self.lock.lock().unwrap();
        let path = facts_path(&self.workspace, scope);
        let mut store = FactStore::load(&path)?;

        if store.get(&id).is_none() {
            return Ok(format!("no such fact: {id}"));
        }

        store.soft_delete(&id, "__forgotten__");
        store.save(&path)?;
        Ok(format!("forgotten fact {id}"))
    }
}

pub struct UpdateFact {
    workspace: PathBuf,
    lock: Mutex<()>,
}

impl UpdateFact {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            lock: Mutex::new(()),
        }
    }
}

#[async_trait]
impl Tool for UpdateFact {
    fn is_deferred(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "update_fact".into(),
            description: "Update an existing fact with new text. Creates a new version and links the old one as superseded.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "The ID of the fact to update (e.g. F3)"
                    },
                    "new_text": {
                        "type": "string",
                        "description": "The new text for the fact"
                    },
                    "scope": {
                        "type": "string",
                        "description": "Scope: 'workspace' (default) or 'global'",
                        "default": "workspace"
                    }
                },
                "required": ["id", "new_text"]
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let id = arguments["id"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "update_fact".into(),
                message: "missing required parameter: id".to_string(),
            })?
            .to_string();

        let new_text = arguments["new_text"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "update_fact".into(),
                message: "missing required parameter: new_text".to_string(),
            })?
            .to_string();

        let scope = arguments["scope"].as_str().unwrap_or("workspace");

        let _guard = self.lock.lock().unwrap();
        let path = facts_path(&self.workspace, scope);
        let mut store = FactStore::load(&path)?;

        let existing = store.get(&id).ok_or_else(|| Error::BadToolArgs {
            name: "update_fact".into(),
            message: format!("no such fact: {id}"),
        })?;

        let new_id = store.next_id();
        let tags = existing.tags.clone();
        let source = existing.source.clone();

        // Soft-delete the old fact
        store.soft_delete(&id, &new_id);

        // Add the new version
        let now = chrono_now_rfc3339();
        store.facts.push(Fact {
            id: new_id.clone(),
            text: new_text,
            tags,
            source,
            created_at: now.clone(),
            last_accessed: now,
            access_count: 0,
            superseded_by: None,
        });

        store.save(&path)?;
        Ok(format!("updated fact {id} -> {new_id}"))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a temporary workspace dir.
    fn tmp_workspace() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = tmp.path().to_path_buf();
        (tmp, ws)
    }

    #[test]
    fn test_a_remember_recall_roundtrip() {
        let (_tmp, ws) = tmp_workspace();
        let remember = RememberFact::new(&ws);
        let recall = RecallFact::new(&ws);

        // Remember a fact
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(remember.execute(json!({
                "text": "Rust uses the Result type for error handling",
                "tags": ["rust", "error-handling"],
                "source": "agent"
            })));
        assert!(result.is_ok());
        let msg = result.unwrap();
        assert!(msg.starts_with("saved fact F"), "got: {msg}");

        // Recall by query
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(recall.execute(json!({"query": "Result type"})));
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("F1"), "output: {output}");
        assert!(output.contains("Result type"), "output: {output}");
        assert!(output.contains("[rust,error-handling]"), "output: {output}");
    }

    #[test]
    fn test_b_duplicate_detection() {
        let (_tmp, ws) = tmp_workspace();
        let remember = RememberFact::new(&ws);

        // First fact
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(remember.execute(json!({
                "text": "Rust uses the Result type for error handling",
                "tags": ["rust"]
            })))
            .unwrap();

        // Similar fact — should be detected as duplicate
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(remember.execute(json!({
                "text": "Rust uses Result for error handling",
                "tags": ["rust"]
            })));
        assert!(result.is_ok());
        let msg = result.unwrap();
        assert!(msg.contains("duplicate of F1"), "got: {msg}");
    }

    #[test]
    fn test_c_tag_filtering() {
        let (_tmp, ws) = tmp_workspace();
        let remember = RememberFact::new(&ws);
        let recall = RecallFact::new(&ws);

        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(remember.execute(json!({
                "text": "Python uses exceptions for error handling",
                "tags": ["python"]
            })))
            .unwrap();

        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(remember.execute(json!({
                "text": "Rust uses Result for error handling",
                "tags": ["rust"]
            })))
            .unwrap();

        // Filter by tag
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(recall.execute(json!({"tag": "rust"})));
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("F2"), "output: {output}");
        assert!(!output.contains("F1"), "output: {output}");
    }

    #[test]
    fn test_d_access_count_increments() {
        let (_tmp, ws) = tmp_workspace();
        let remember = RememberFact::new(&ws);
        let recall = RecallFact::new(&ws);

        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(remember.execute(json!({
                "text": "Important fact",
                "tags": ["test"]
            })))
            .unwrap();

        // Recall multiple times
        for _ in 0..3 {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(recall.execute(json!({"query": "Important"})))
                .unwrap();
        }

        // Load store and check access count
        let path = facts_path(&ws, "workspace");
        let store = FactStore::load(&path).unwrap();
        let fact = store.get("F1").unwrap();
        assert_eq!(fact.access_count, 3, "access count should be 3");
    }

    #[test]
    fn test_e_eviction_at_cap() {
        let (_tmp, ws) = tmp_workspace();
        let remember = RememberFact::new(&ws);

        // Add FACTS_CAP + 10 facts
        for i in 0..FACTS_CAP + 10 {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(remember.execute(json!({
                    "text": format!("Fact number {}", i),
                    "tags": ["test"]
                })))
                .unwrap();
        }

        // Load store and check active count
        let path = facts_path(&ws, "workspace");
        let store = FactStore::load(&path).unwrap();
        let active = store.active_facts();
        assert!(
            active.len() <= FACTS_CAP,
            "active facts: {} > cap {}",
            active.len(),
            FACTS_CAP
        );
    }

    #[test]
    fn test_f_forget_marks_superseded() {
        let (_tmp, ws) = tmp_workspace();
        let remember = RememberFact::new(&ws);
        let forget = ForgetFact::new(&ws);

        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(remember.execute(json!({
                "text": "Something to forget",
                "tags": ["test"]
            })))
            .unwrap();

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(forget.execute(json!({"id": "F1"})));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "forgotten fact F1");

        // Verify it's no longer active
        let path = facts_path(&ws, "workspace");
        let store = FactStore::load(&path).unwrap();
        let fact = store.get("F1").unwrap();
        assert_eq!(fact.superseded_by, Some("__forgotten__".to_string()));
        assert!(!fact.is_active());
    }

    #[test]
    fn test_g_update_fact_creates_new_version() {
        let (_tmp, ws) = tmp_workspace();
        let remember = RememberFact::new(&ws);
        let update = UpdateFact::new(&ws);

        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(remember.execute(json!({
                "text": "Old version of the fact",
                "tags": ["test"]
            })))
            .unwrap();

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(update.execute(json!({
                "id": "F1",
                "new_text": "New improved version of the fact"
            })));
        assert!(result.is_ok());
        let msg = result.unwrap();
        assert!(msg.contains("F1 -> F2"), "got: {msg}");

        // Verify old fact is superseded
        let path = facts_path(&ws, "workspace");
        let store = FactStore::load(&path).unwrap();
        let old = store.get("F1").unwrap();
        assert_eq!(old.superseded_by, Some("F2".to_string()));
        let new_fact = store.get("F2").unwrap();
        assert_eq!(new_fact.text, "New improved version of the fact");
        assert!(new_fact.is_active());
    }

    #[test]
    fn test_h_search_scoring() {
        let (_tmp, ws) = tmp_workspace();
        let remember = RememberFact::new(&ws);

        // Add facts with varying relevance
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(remember.execute(json!({
                "text": "Rust is a systems programming language",
                "tags": ["rust"]
            })))
            .unwrap();

        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(remember.execute(json!({
                "text": "Python is great for data science",
                "tags": ["python"]
            })))
            .unwrap();

        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(remember.execute(json!({
                "text": "Rust has excellent performance characteristics",
                "tags": ["rust", "performance"]
            })))
            .unwrap();

        // Search for "rust"
        let path = facts_path(&ws, "workspace");
        let store = FactStore::load(&path).unwrap();
        let active: Vec<&Fact> = store.active_facts();
        let results = search_facts(&active, "rust", None, 10);

        assert_eq!(results.len(), 2, "should find 2 rust facts");
        // Both should have "rust" in them
        for r in &results {
            assert!(
                r.fact.text.to_lowercase().contains("rust"),
                "fact: {}",
                r.fact.text
            );
        }
    }

    #[test]
    fn test_i_scope_isolation() {
        let (_tmp, ws) = tmp_workspace();

        // Override HOME to the temp dir so global-scope facts land in a
        // writable location (macOS security policy may block writes to
        // ~/.recursive/memory/ in test environments). The guard holds
        // the cross-module env lock and restores HOME on drop, so this
        // test no longer pollutes parallel tests that read $HOME.
        let home = ws.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let _g = crate::test_util::PinnedHome::new(&home);

        let remember = RememberFact::new(&ws);
        let recall = RecallFact::new(&ws);

        // Add workspace fact
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(remember.execute(json!({
                "text": "Workspace-specific fact",
                "scope": "workspace"
            })))
            .unwrap();

        // Add global fact
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(remember.execute(json!({
                "text": "Global fact",
                "scope": "global"
            })))
            .unwrap();

        // Recall workspace only
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(recall.execute(json!({"scope": "workspace", "query": "fact"})));
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("Workspace-specific"), "output: {output}");
        assert!(!output.contains("Global"), "output: {output}");

        // Recall global only
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(recall.execute(json!({"scope": "global", "query": "fact"})));
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("Global"), "output: {output}");
        assert!(!output.contains("Workspace-specific"), "output: {output}");
    }

    #[test]
    fn test_j_tokenize_and_stop_words() {
        let tokens = tokenize("Hello, World! This is a test.");
        assert!(tokens.contains(&"hello".to_string()));
        assert!(tokens.contains(&"world".to_string()));
        assert!(tokens.contains(&"test".to_string()));
        // Stop words should still be in tokenize output (filtering is done in search)
        assert!(tokens.contains(&"this".to_string()));
    }

    #[test]
    fn test_k_jaccard_similarity() {
        let sim = jaccard_similarity(
            "Rust uses Result for error handling",
            "Rust uses Result for error handling",
        );
        assert!(
            (sim - 1.0).abs() < 0.01,
            "identical texts should have sim=1, got {sim}"
        );

        let sim = jaccard_similarity("Rust uses Result", "Python uses exceptions");
        assert!(sim < 0.5, "different texts should have low sim, got {sim}");

        let sim = jaccard_similarity("", "");
        assert!(
            (sim - 1.0).abs() < 0.01,
            "empty strings should have sim=1, got {sim}"
        );
    }

    #[test]
    fn test_l_rfc3339_to_secs() {
        let secs = rfc3339_to_secs("2026-05-25T12:00:00Z").unwrap();
        assert!(secs > 0.0, "should parse to positive seconds");
    }

    #[test]
    fn test_m_facts_summary_empty() {
        let (_tmp, ws) = tmp_workspace();
        // Pin HOME to an empty temp dir so global facts on the developer's
        // real machine don't leak into this test (facts_summary now merges
        // both workspace and global scopes).
        let fake_home = _tmp.path().join("home");
        std::fs::create_dir_all(&fake_home).unwrap();
        let _pin = crate::test_util::PinnedHome::new(&fake_home);
        let summary = facts_summary(&ws, 5);
        assert_eq!(summary, "");
    }

    #[test]
    fn test_n_facts_summary_with_facts() {
        let (_tmp, ws) = tmp_workspace();
        let remember = RememberFact::new(&ws);

        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(remember.execute(json!({
                "text": "First fact",
                "tags": ["a"]
            })))
            .unwrap();

        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(remember.execute(json!({
                "text": "Second fact",
                "tags": ["b"]
            })))
            .unwrap();

        let summary = facts_summary(&ws, 5);
        assert!(!summary.is_empty());
        assert!(summary.contains("F1"));
        assert!(summary.contains("F2"));
        assert!(summary.contains("First fact"));
        assert!(summary.contains("Second fact"));
    }
}
