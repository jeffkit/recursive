//! Derived session-search index (issue #131, borrowed from DSH `session-query`).
//!
//! Sessions on disk are append-only JSONL transcripts. That is the right
//! storage format and the wrong query format: answering "which past session
//! mentioned X" means re-parsing every log. [`SessionIndex`] is a **discardable
//! read model** over those logs — a SQLite FTS5 database that holds nothing
//! that cannot be re-derived from the session directories.
//!
//! Three properties make it safe to keep around:
//!
//! - **self-identifying header** — the database carries
//!   [`INDEX_APPLICATION_ID`] + [`INDEX_FORMAT_VERSION`]. A database that does
//!   not carry the current stamp is not migrated, it is **rebuilt in place**
//!   ([`SessionIndex::open`]), because a derived index is never the source of
//!   truth.
//! - **revision-keyed cold reads** — a session is re-indexed only when its
//!   `transcript.jsonl` / `.meta.json` stat revision changed
//!   ([`stat_revision`]). Unchanged logs are not re-read.
//! - **active leases outlive LRU pressure** — sessions a caller declares active
//!   ([`SessionIndex::lease`]) are held in a TEMP table that dies with the
//!   connection and are never evicted by the cold-read cache's LRU
//!   ([`ColdReadCache`]), and are always re-read on refresh (an active
//!   transcript is still growing).
//!
//! The database lives at `<user-workspace-dir>/session-index.sqlite3` and is
//! created `0600` inside the workspace's `0700` data directory — a session
//! transcript is private conversation data.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension};

use crate::error::{Error, Result};
use crate::session::reader::SessionReader;
use crate::session::serialize::LoadedEntry;

/// SQLite `application_id` stamp: "RCS1" (recursive session index v1).
pub const INDEX_APPLICATION_ID: i32 = 0x5243_5331;

/// Schema version of the derived database. Bump on any change that makes an
/// existing database semantically stale; [`SessionIndex::open`] then rebuilds.
pub const INDEX_FORMAT_VERSION: i32 = 1;

/// Hard ceiling on how many rows a single search may return (DSH caps model
/// tool results at 100 for the same reason: a search is a lookup, not a dump).
pub const MAX_SEARCH_RESULTS: usize = 100;

/// How many parsed transcripts the cold-read cache keeps before evicting the
/// least-recently-used unleased entry.
pub const DEFAULT_COLD_CACHE_CAPACITY: usize = 16;

const INDEX_FILE_NAME: &str = "session-index.sqlite3";
const TRANSCRIPT_FILE: &str = "transcript.jsonl";
const META_FILE: &str = ".meta.json";

/// One session header, as stored in the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHit {
    pub session_id: String,
    pub goal: String,
    pub name: Option<String>,
    pub model: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    pub message_count: u64,
}

/// One matching transcript event, with a short window around the match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventHit {
    pub session_id: String,
    pub index: usize,
    pub entry_id: String,
    pub role: String,
    pub tool_name: Option<String>,
    pub timestamp: String,
    pub snippet: String,
    /// `true` when the owning session currently holds an active lease.
    pub active: bool,
}

/// One transcript event read back in full (no snippet truncation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEvent {
    pub index: usize,
    pub entry_id: String,
    pub role: String,
    pub tool_name: Option<String>,
    pub timestamp: String,
    pub content: String,
}

/// What a [`SessionIndex::refresh`] pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RefreshReport {
    /// Sessions (re-)read and written to the index.
    pub indexed: usize,
    /// Sessions whose stat revision matched and that were therefore not read.
    pub skipped: usize,
    /// Sessions dropped from the index because their directory disappeared or
    /// stopped being a session.
    pub removed: usize,
}

/// LRU cache of parsed transcripts, pinned by active leases.
///
/// The cache key is the pair (this instance's identity, stat revision): the
/// cache lives inside one [`SessionIndex`], so a revision can never be
/// mistaken for another instance's read of the same log.
#[derive(Debug)]
struct ColdReadCache<T> {
    capacity: usize,
    clock: u64,
    entries: HashMap<String, ColdEntry<T>>,
}

#[derive(Debug)]
struct ColdEntry<T> {
    revision: String,
    value: Arc<T>,
    leases: u32,
    used_at: u64,
}

impl<T> ColdReadCache<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            clock: 0,
            entries: HashMap::new(),
        }
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// Return the cached value when the revision still matches. A hit refreshes
    /// the entry's LRU position.
    fn get(&mut self, session_id: &str, revision: &str) -> Option<Arc<T>> {
        let tick = self.tick();
        let entry = self.entries.get_mut(session_id)?;
        if entry.revision != revision {
            return None;
        }
        entry.used_at = tick;
        Some(Arc::clone(&entry.value))
    }

    /// Insert an entry, evicting the least-recently-used **unleased** entries
    /// until the cache is within capacity. Returns the evicted session ids.
    fn put(&mut self, session_id: &str, revision: &str, value: Arc<T>) -> Vec<String> {
        let tick = self.tick();
        self.entries.insert(
            session_id.to_string(),
            ColdEntry {
                revision: revision.to_string(),
                value,
                leases: 0,
                used_at: tick,
            },
        );
        self.evict()
    }

    fn evict(&mut self) -> Vec<String> {
        let mut evicted = Vec::new();
        while self.entries.len() > self.capacity {
            let victim = self
                .entries
                .iter()
                .filter(|(_, e)| e.leases == 0)
                .min_by_key(|(_, e)| e.used_at)
                .map(|(k, _)| k.clone());
            // Every remaining entry is leased: honour the lease over capacity.
            let Some(victim) = victim else { break };
            self.entries.remove(&victim);
            evicted.push(victim);
        }
        evicted
    }

    /// Pin an existing entry against eviction. Returns `true` when an entry
    /// was pinned (leasing an absent session is a no-op — the lease itself
    /// lives in the index's TEMP table).
    fn lease(&mut self, session_id: &str) -> bool {
        match self.entries.get_mut(session_id) {
            Some(entry) => {
                entry.leases += 1;
                true
            }
            None => false,
        }
    }

    fn release(&mut self, session_id: &str) {
        if let Some(entry) = self.entries.get_mut(session_id) {
            entry.leases = entry.leases.saturating_sub(1);
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// A derived, discardable FTS5 index over this workspace's session logs.
pub struct SessionIndex {
    workspace: PathBuf,
    path: PathBuf,
    conn: Connection,
    reads: ColdReadCache<Vec<LoadedEntry>>,
}

impl SessionIndex {
    /// Open (creating on first use) the derived index for `workspace`.
    ///
    /// A database whose `application_id` or `user_version` stamp does not
    /// match this build is rebuilt in place: every table is dropped and
    /// recreated, because the index is derived data that a later
    /// [`SessionIndex::refresh`] repopulates.
    pub fn open(workspace: &Path) -> Result<Self> {
        let dir = crate::paths::user_workspace_dir(workspace)?;
        restrict_dir(&dir);
        let path = dir.join(INDEX_FILE_NAME);
        let conn = Connection::open(&path).map_err(storage_err)?;
        restrict_file(&path);
        let mut index = Self {
            workspace: workspace.to_path_buf(),
            path,
            conn,
            reads: ColdReadCache::new(DEFAULT_COLD_CACHE_CAPACITY),
        };
        index.ensure_schema()?;
        Ok(index)
    }

    /// Path of the derived database (0600).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Workspace whose sessions this index was built for.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Stamp the header and (re)create the schema, rebuilding when the stamp
    /// on disk was written by a different application or format version.
    fn ensure_schema(&mut self) -> Result<()> {
        let application_id: i32 = self
            .conn
            .query_row("PRAGMA application_id", [], |row| row.get(0))?;
        let user_version: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if application_id != INDEX_APPLICATION_ID || user_version != INDEX_FORMAT_VERSION {
            self.rebuild()?;
        }
        Ok(())
    }

    /// Drop every index table and recreate them. `TEMP` tables (active leases)
    /// belong to this connection and are recreated with the schema.
    fn rebuild(&mut self) -> Result<()> {
        self.conn
            .execute_batch("DROP TABLE IF EXISTS events; DROP TABLE IF EXISTS sessions;")?;
        self.create_schema()
    }

    fn create_schema(&mut self) -> Result<()> {
        self.conn.execute_batch(&format!(
            "PRAGMA application_id = {INDEX_APPLICATION_ID};
             PRAGMA user_version = {INDEX_FORMAT_VERSION};
             CREATE TABLE IF NOT EXISTS sessions (
                 session_id    TEXT PRIMARY KEY,
                 dir           TEXT NOT NULL,
                 goal          TEXT NOT NULL DEFAULT '',
                 name          TEXT,
                 model         TEXT NOT NULL DEFAULT '',
                 status        TEXT NOT NULL DEFAULT '',
                 created_at    TEXT NOT NULL DEFAULT '',
                 updated_at    TEXT NOT NULL DEFAULT '',
                 message_count INTEGER NOT NULL DEFAULT 0,
                 first_uuid    TEXT,
                 revision      TEXT NOT NULL DEFAULT ''
             );
             CREATE VIRTUAL TABLE IF NOT EXISTS events USING fts5(
                 content,
                 session_id UNINDEXED,
                 entry_index UNINDEXED,
                 entry_id UNINDEXED,
                 role UNINDEXED,
                 tool_name,
                 timestamp UNINDEXED,
                 tokenize = 'trigram'
             );
             CREATE TEMP TABLE IF NOT EXISTS active_sessions (
                 session_id  TEXT PRIMARY KEY,
                 instance_id TEXT NOT NULL,
                 leased_at   INTEGER NOT NULL
             );"
        ))?;
        Ok(())
    }

    /// Re-index every session whose directory or stat revision changed, drop
    /// rows for sessions that no longer exist, and report what happened.
    ///
    /// Sessions holding an active lease are always re-read: their transcript is
    /// being appended to right now, so the flushed revision is not the whole
    /// story.
    pub fn refresh(&mut self) -> Result<RefreshReport> {
        let dirs = SessionReader::list_sessions(&self.workspace).map_err(storage_err)?;
        let active = self.active_sessions()?;
        let mut report = RefreshReport::default();
        let mut seen: Vec<String> = Vec::with_capacity(dirs.len());

        for dir in dirs {
            let Some(session_id) = dir.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                continue;
            };
            let revision = match stat_revision(&dir) {
                Some(rev) => rev,
                // A session directory without a transcript is not indexable.
                None => continue,
            };
            seen.push(session_id.clone());
            let is_active = active.iter().any(|a| a == &session_id);
            if !is_active {
                let dir_str = dir.to_string_lossy();
                if let Some((stored_dir, stored_revision)) = self.stored_state(&session_id)? {
                    // The directory is part of the revision: a session root
                    // that moved keeps every mtime and size, so a stat-only
                    // match would leave `transcript()` reading the old path.
                    if stored_revision == revision && stored_dir == dir_str {
                        report.skipped += 1;
                        continue;
                    }
                }
            }
            self.index_session(&session_id, &dir, &revision)?;
            report.indexed += 1;
        }

        report.removed = self.sweep(&seen, &active)?;
        Ok(report)
    }

    /// Read one session directory into the index, replacing any previous rows.
    fn index_session(&mut self, session_id: &str, dir: &Path, revision: &str) -> Result<()> {
        let meta = SessionReader::load_meta(dir).map_err(storage_err)?;
        let history = SessionReader::load_full_history(dir).map_err(storage_err)?;

        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM events WHERE session_id = ?1",
            rusqlite::params![session_id],
        )?;

        let mut entry_index = 0usize;
        let mut first_uuid: Option<String> = None;
        {
            let mut insert = tx.prepare(
                "INSERT INTO events(content, session_id, entry_index, entry_id, role, tool_name, timestamp)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for entry in &history {
                let LoadedEntry::Message(msg) = entry else {
                    // Compaction boundaries are relation markers, not events.
                    continue;
                };
                if first_uuid.is_none() && !msg.uuid.is_empty() {
                    first_uuid = Some(msg.uuid.clone());
                }
                insert.execute(rusqlite::params![
                    msg.content,
                    session_id,
                    entry_index.to_string(),
                    msg.id,
                    msg.role,
                    tool_names(msg),
                    msg.timestamp,
                ])?;
                entry_index += 1;
            }
        }

        tx.execute(
            "INSERT INTO sessions(session_id, dir, goal, name, model, status, created_at, updated_at,
                                  message_count, first_uuid, revision)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(session_id) DO UPDATE SET
                 dir = excluded.dir, goal = excluded.goal, name = excluded.name,
                 model = excluded.model, status = excluded.status,
                 created_at = excluded.created_at, updated_at = excluded.updated_at,
                 message_count = excluded.message_count, first_uuid = excluded.first_uuid,
                 revision = excluded.revision",
            rusqlite::params![
                session_id,
                dir.to_string_lossy(),
                meta.goal,
                meta.name,
                meta.model,
                meta.status.to_string(),
                meta.created_at,
                meta.updated_at,
                meta.message_count as i64,
                first_uuid,
                revision,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Drop index rows for sessions that are gone from disk. An actively leased
    /// session is kept even if its directory could not be listed (a live writer
    /// holds the session lock through directory churn).
    fn sweep(&self, seen: &[String], active: &[String]) -> Result<usize> {
        let known: Vec<String> = {
            let mut stmt = self.conn.prepare("SELECT session_id FROM sessions")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        let mut removed = 0usize;
        for session_id in known {
            if seen.contains(&session_id) || active.contains(&session_id) {
                continue;
            }
            self.conn.execute(
                "DELETE FROM events WHERE session_id = ?1",
                rusqlite::params![&session_id],
            )?;
            self.conn.execute(
                "DELETE FROM sessions WHERE session_id = ?1",
                rusqlite::params![&session_id],
            )?;
            removed += 1;
        }
        Ok(removed)
    }

    /// `(dir, revision)` currently stored for `session_id`, if any.
    fn stored_state(&self, session_id: &str) -> Result<Option<(String, String)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT dir, revision FROM sessions WHERE session_id = ?1",
                rusqlite::params![session_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?)
    }

    /// Full-text search over transcript events, ordered by session then
    /// position so results are reproducible.
    ///
    /// Terms of three characters or more go through the FTS5 trigram index (a
    /// case-insensitive substring match); shorter terms fall back to `LIKE`, so
    /// two-character CJK queries still work.
    pub fn search_events(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
    ) -> Result<Vec<EventHit>> {
        let terms = query_terms(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let limit = limit.clamp(1, MAX_SEARCH_RESULTS);
        let active = self.active_sessions()?;

        let mut clauses: Vec<String> = Vec::new();
        let mut values: Vec<Value> = Vec::new();
        let long: Vec<&String> = terms.iter().filter(|t| t.chars().count() >= 3).collect();
        if !long.is_empty() {
            clauses.push("events MATCH ?".to_string());
            values.push(Value::Text(match_expression(&long)));
        }
        for term in terms.iter().filter(|t| t.chars().count() < 3) {
            clauses.push("content LIKE ? ESCAPE '\\'".to_string());
            values.push(Value::Text(like_pattern(term)));
        }
        if let Some(sid) = session_id {
            clauses.push("session_id = ?".to_string());
            values.push(Value::Text(sid.to_string()));
        }
        values.push(Value::Integer(limit as i64));

        let sql = format!(
            "SELECT session_id, entry_index, entry_id, role, tool_name, timestamp, content
             FROM events WHERE {} ORDER BY session_id, CAST(entry_index AS INTEGER) LIMIT ?",
            clauses.join(" AND ")
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(values.iter()), |row| {
            let session_id: String = row.get(0)?;
            let index: String = row.get(1)?;
            let content: String = row.get(6)?;
            Ok(EventHit {
                active: false,
                session_id,
                index: index.parse().unwrap_or(0),
                entry_id: row.get(2)?,
                role: row.get(3)?,
                tool_name: row.get(4)?,
                timestamp: row.get(5)?,
                snippet: snippet(&content, &terms, SNIPPET_CHARS),
            })
        })?;
        let mut hits = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        for hit in &mut hits {
            hit.active = active.iter().any(|a| a == &hit.session_id);
        }
        Ok(hits)
    }

    /// Search session headers (goal + display name).
    pub fn search_sessions(&self, query: &str, limit: usize) -> Result<Vec<SessionHit>> {
        let terms = query_terms(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let limit = limit.clamp(1, MAX_SEARCH_RESULTS);
        let mut clauses: Vec<String> = Vec::new();
        let mut values: Vec<Value> = Vec::new();
        for term in &terms {
            clauses.push("(goal || ' ' || COALESCE(name, '')) LIKE ? ESCAPE '\\'".to_string());
            values.push(Value::Text(like_pattern(term)));
        }
        values.push(Value::Integer(limit as i64));
        let sql = format!(
            "SELECT session_id, goal, name, model, status, created_at, updated_at, message_count
             FROM sessions WHERE {} ORDER BY updated_at DESC, session_id LIMIT ?",
            clauses.join(" AND ")
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(values.iter()), row_to_session)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// All indexed sessions, most recently updated first.
    pub fn sessions(&self, limit: usize) -> Result<Vec<SessionHit>> {
        let limit = limit.clamp(1, MAX_SEARCH_RESULTS);
        let mut stmt = self.conn.prepare(
            "SELECT session_id, goal, name, model, status, created_at, updated_at, message_count
             FROM sessions ORDER BY updated_at DESC, session_id LIMIT ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![limit as i64], row_to_session)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// One session header by id.
    pub fn session(&self, session_id: &str) -> Result<Option<SessionHit>> {
        Ok(self
            .conn
            .query_row(
                "SELECT session_id, goal, name, model, status, created_at, updated_at, message_count
                 FROM sessions WHERE session_id = ?1",
                rusqlite::params![session_id],
                row_to_session,
            )
            .optional()?)
    }

    /// Directory of an indexed session.
    pub fn session_dir(&self, session_id: &str) -> Result<Option<PathBuf>> {
        Ok(self
            .conn
            .query_row(
                "SELECT dir FROM sessions WHERE session_id = ?1",
                rusqlite::params![session_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(PathBuf::from))
    }

    /// Sessions whose first message uuid matches `session_id`'s — i.e. logs
    /// that start from the same origin transcript (a verbatim copy / fork).
    /// Recursive persists no explicit fork lineage, so transcript identity is
    /// the derived signal; a session that was rewritten on the way in (the
    /// AG-UI legacy migration) simply does not appear here.
    pub fn same_origin(&self, session_id: &str) -> Result<Vec<String>> {
        let Some(uuid) = self
            .conn
            .query_row(
                "SELECT first_uuid FROM sessions WHERE session_id = ?1",
                rusqlite::params![session_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
        else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare(
            "SELECT session_id FROM sessions WHERE first_uuid = ?1 AND session_id <> ?2
             ORDER BY updated_at DESC, session_id",
        )?;
        let rows = stmt.query_map(rusqlite::params![uuid, session_id], |row| {
            row.get::<_, String>(0)
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Read indexed events of one session in transcript order.
    pub fn read_events(
        &self,
        session_id: &str,
        from: usize,
        to: usize,
    ) -> Result<Vec<StoredEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT entry_index, entry_id, role, tool_name, timestamp, content FROM events
             WHERE session_id = ?1 AND CAST(entry_index AS INTEGER) >= ?2
               AND CAST(entry_index AS INTEGER) <= ?3
             ORDER BY CAST(entry_index AS INTEGER)",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![session_id, from as i64, to as i64],
            |row| {
                let index: String = row.get(0)?;
                Ok(StoredEvent {
                    index: index.parse().unwrap_or(0),
                    entry_id: row.get(1)?,
                    role: row.get(2)?,
                    tool_name: row.get(3)?,
                    timestamp: row.get(4)?,
                    content: row.get(5)?,
                })
            },
        )?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Number of events indexed for a session.
    pub fn event_count(&self, session_id: &str) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM events WHERE session_id = ?1",
            rusqlite::params![session_id],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// Mark a session active. Active sessions are re-read on every refresh and
    /// their cold-read entries are pinned against LRU eviction. The lease lives
    /// in a TEMP table, so it dies with this instance.
    pub fn lease(&mut self, session_id: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO active_sessions(session_id, instance_id, leased_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) DO NOTHING",
            rusqlite::params![session_id, self.instance_id(), now_millis()],
        )?;
        // Pin an already-parsed transcript; an unparsed session is pinned on
        // its first `transcript` call instead.
        self.reads.lease(session_id);
        Ok(())
    }

    /// Drop a session's active lease.
    pub fn release(&mut self, session_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM active_sessions WHERE session_id = ?1",
            rusqlite::params![session_id],
        )?;
        self.reads.release(session_id);
        Ok(())
    }

    /// Sessions currently leased by this instance.
    pub fn active_sessions(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT session_id FROM active_sessions ORDER BY session_id")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// How many parsed transcripts the cold-read cache currently holds.
    pub fn cold_cache_len(&self) -> usize {
        self.reads.len()
    }

    /// Load (and cache) a session's full history, boundaries included.
    ///
    /// The cache is keyed by the transcript's stat revision, so an unchanged
    /// log is parsed once per instance.
    pub fn transcript(&mut self, session_id: &str) -> Result<Arc<Vec<LoadedEntry>>> {
        let dir = self
            .session_dir(session_id)?
            .ok_or_else(|| Error::NotFound(format!("session {session_id}")))?;
        let revision = stat_revision(&dir).unwrap_or_default();
        if let Some(hit) = self.reads.get(session_id, &revision) {
            return Ok(hit);
        }
        let history = SessionReader::load_full_history(&dir).map_err(storage_err)?;
        let value = Arc::new(history);
        self.reads.put(session_id, &revision, Arc::clone(&value));
        // A leased session pins its parsed transcript against eviction.
        if self.active_sessions()?.iter().any(|a| a == session_id) {
            self.reads.lease(session_id);
        }
        Ok(value)
    }

    fn instance_id(&self) -> String {
        std::process::id().to_string()
    }
}

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionHit> {
    Ok(SessionHit {
        session_id: row.get(0)?,
        goal: row.get(1)?,
        name: row.get(2)?,
        model: row.get(3)?,
        status: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
        message_count: row.get::<_, i64>(7)? as u64,
    })
}

/// Characters of context kept on each side of a snippet match.
const SNIPPET_CHARS: usize = 160;

/// Split a query into search terms (whitespace separated, empties dropped).
fn query_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(|t| t.to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Build an FTS5 query: one quoted phrase per term, joined by the implicit AND
/// so every term must appear (`trigram` gives substring semantics inside a
/// phrase).
fn match_expression(terms: &[&String]) -> String {
    terms
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `LIKE` pattern for a term, `%`/`_`/`\` escaped (`ESCAPE '\'`).
fn like_pattern(term: &str) -> String {
    let mut out = String::with_capacity(term.len() + 2);
    out.push('%');
    for ch in term.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('%');
    out
}

/// Tool names called by an assistant entry, comma separated (empty when none).
fn tool_names(entry: &crate::session::TranscriptEntry) -> String {
    entry
        .tool_calls
        .iter()
        .map(|c| c.name.clone())
        .collect::<Vec<_>>()
        .join(",")
}

/// A window of `width` characters around the first matching term.
fn snippet(content: &str, terms: &[String], width: usize) -> String {
    let haystack = content.to_ascii_lowercase();
    let position = terms
        .iter()
        .filter_map(|t| haystack.find(&t.to_ascii_lowercase()))
        .min()
        .unwrap_or(0);
    let mut start = position.saturating_sub(width / 2);
    while start > 0 && !content.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (start + width).min(content.len());
    while end < content.len() && !content.is_char_boundary(end) {
        end += 1;
    }
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.push_str(&content[start..end]);
    if end < content.len() {
        out.push('…');
    }
    out
}

/// Stat revision of a session directory: `transcript.jsonl` + `.meta.json`
/// mtime (nanoseconds) and size. Any write to either file changes it, so an
/// unchanged revision means the index rows are still current.
fn stat_revision(dir: &Path) -> Option<String> {
    let transcript = std::fs::metadata(dir.join(TRANSCRIPT_FILE)).ok()?;
    let meta = std::fs::metadata(dir.join(META_FILE)).ok()?;
    let transcript_mtime = mtime_nanos(&transcript);
    let meta_mtime = mtime_nanos(&meta);
    Some(format!(
        "{transcript_mtime}:{}:{meta_mtime}:{}",
        transcript.len(),
        meta.len()
    ))
}

fn mtime_nanos(meta: &std::fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Private session data: the index directory is 0700 and the database 0600.
fn restrict_dir(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = dir;
}

fn restrict_file(file: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = file;
}

fn storage_err(e: impl std::fmt::Display) -> Error {
    Error::Storage {
        message: e.to_string(),
    }
}

/// The derived index is the only code that talks to SQLite with `?`; every
/// failure is a storage failure to the caller. (`rusqlite` is an optional
/// dependency, so the conversion is gated with the module that needs it.)
impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Storage {
            message: e.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;
    use crate::session::{SessionStatus, SessionWriter};
    use crate::test_util::IsolatedWorkspace;

    fn write_session(ws: &Path, goal: &str, user: &str, assistant: &str) -> String {
        let mut writer = SessionWriter::create(ws, goal, "deepseek-chat", "deepseek").unwrap();
        writer
            .append(&Message::system("sys".to_string()), None, None)
            .unwrap();
        writer
            .append(&Message::user(user.to_string()), None, None)
            .unwrap();
        writer
            .append(&Message::assistant(assistant.to_string()), None, None)
            .unwrap();
        writer.finish(SessionStatus::Completed).unwrap();
        writer.session_id().to_string()
    }

    fn open_index(ws: &Path) -> SessionIndex {
        SessionIndex::open(ws).expect("index opens")
    }

    #[test]
    fn refresh_indexes_sessions_and_search_finds_content() {
        let env = IsolatedWorkspace::new();
        write_session(
            env.path(),
            "fix the",
            "hello parser world",
            "the parser is fixed",
        );
        let mut index = open_index(env.path());
        let report = index.refresh().unwrap();
        assert_eq!(report.indexed, 1);
        assert_eq!(report.skipped, 0);

        let hits = index.search_events("parser", 10, None).unwrap();
        assert_eq!(hits.len(), 2, "user and assistant both mention the parser");
        assert_eq!(hits[0].role, "user");
        assert!(hits[0].snippet.contains("parser"));
    }

    #[test]
    fn search_events_matches_two_char_cjk_terms() {
        let env = IsolatedWorkspace::new();
        write_session(
            env.path(),
            "session retrieval",
            "看一下检索索引",
            "索引已重建",
        );
        let mut index = open_index(env.path());
        index.refresh().unwrap();

        // Two characters: below the trigram floor, so the LIKE fallback runs.
        let hits = index.search_events("检索", 10, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].role, "user");
    }

    #[test]
    fn search_events_can_be_scoped_to_one_session() {
        let env = IsolatedWorkspace::new();
        let first = write_session(env.path(), "one", "shared token", "done one");
        write_session(env.path(), "two", "shared token", "done two");
        let mut index = open_index(env.path());
        index.refresh().unwrap();

        let all = index.search_events("shared", 10, None).unwrap();
        assert_eq!(all.len(), 2);
        let scoped = index.search_events("shared", 10, Some(&first)).unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].session_id, first);
    }

    #[test]
    fn search_limit_is_capped_at_max_results() {
        let env = IsolatedWorkspace::new();
        let mut writer = SessionWriter::create(env.path(), "cap", "m", "p").unwrap();
        for i in 0..120 {
            writer
                .append(&Message::user(format!("needle {i}")), None, None)
                .unwrap();
        }
        writer.finish(SessionStatus::Completed).unwrap();
        let mut index = open_index(env.path());
        index.refresh().unwrap();

        let hits = index.search_events("needle", 500, None).unwrap();
        assert_eq!(hits.len(), MAX_SEARCH_RESULTS);
        // A smaller limit is honoured as-is, and zero is clamped up to one.
        assert_eq!(index.search_events("needle", 3, None).unwrap().len(), 3);
        assert_eq!(index.search_events("needle", 0, None).unwrap().len(), 1);
        assert_eq!(index.sessions(0).unwrap().len(), 1);
    }

    #[test]
    fn search_sessions_matches_goal_and_name() {
        let env = IsolatedWorkspace::new();
        let first = write_session(env.path(), "refactor the session index", "a", "b");
        write_session(env.path(), "unrelated chore", "c", "d");
        let mut index = open_index(env.path());
        index.refresh().unwrap();

        let hits = index.search_sessions("session index", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, first);
        assert_eq!(hits[0].status, "completed");
        assert_eq!(hits[0].model, "deepseek-chat");
        assert_eq!(index.sessions(10).unwrap().len(), 2);
    }

    #[test]
    fn unchanged_revision_is_not_re_read() {
        let env = IsolatedWorkspace::new();
        write_session(env.path(), "goal", "hello", "world");
        let mut index = open_index(env.path());
        assert_eq!(index.refresh().unwrap().indexed, 1);
        let second = index.refresh().unwrap();
        assert_eq!(second.indexed, 0);
        assert_eq!(second.skipped, 1);
        // A session that is still on disk is never swept.
        assert_eq!(second.removed, 0);
        assert_eq!(index.sessions(10).unwrap().len(), 1);
    }

    #[test]
    fn refreshed_session_is_picked_up() {
        let env = IsolatedWorkspace::new();
        let ws = env.path().to_path_buf();
        write_session(&ws, "first", "alpha", "reply");
        let mut index = open_index(&ws);
        index.refresh().unwrap();
        assert!(index.search_events("second", 10, None).unwrap().is_empty());

        write_session(&ws, "second", "second needle", "reply");
        let report = index.refresh().unwrap();
        assert_eq!(report.indexed, 1);
        assert_eq!(index.search_events("second", 10, None).unwrap().len(), 1);
    }

    #[test]
    fn a_moved_session_directory_is_re_indexed() {
        let env = IsolatedWorkspace::new();
        let session_id = write_session(env.path(), "goal", "hello", "world");
        let mut index = open_index(env.path());
        index.refresh().unwrap();
        let original = SessionReader::list_sessions(env.path()).unwrap()[0].clone();

        // A new session root: same directory name (so the same session id),
        // same file mtimes and sizes — only the path differs.
        let moved = original
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("moved-slug")
            .join(original.file_name().unwrap());
        std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
        std::fs::rename(&original, &moved).unwrap();

        let report = index.refresh().unwrap();
        assert_eq!(report.indexed, 1, "a moved session must be re-indexed");
        assert_eq!(index.session_dir(&session_id).unwrap(), Some(moved));
        assert!(!index.transcript(&session_id).unwrap().is_empty());
    }

    #[test]
    fn active_session_is_re_indexed_even_when_revision_is_unchanged() {
        let env = IsolatedWorkspace::new();
        let session_id = write_session(env.path(), "goal", "hello", "world");
        let mut index = open_index(env.path());
        index.refresh().unwrap();
        assert_eq!(index.refresh().unwrap().skipped, 1);

        index.lease(&session_id).unwrap();
        assert_eq!(index.active_sessions().unwrap(), vec![session_id.clone()]);
        let report = index.refresh().unwrap();
        assert_eq!(report.indexed, 1);
        assert_eq!(report.skipped, 0);
        assert_eq!(report.removed, 0, "a leased session is never swept");

        index.release(&session_id).unwrap();
        assert!(index.active_sessions().unwrap().is_empty());
    }

    #[test]
    fn a_leased_session_survives_directory_churn() {
        let env = IsolatedWorkspace::new();
        let session_id = write_session(env.path(), "goal", "alpha", "beta");
        let mut index = open_index(env.path());
        index.refresh().unwrap();
        index.lease(&session_id).unwrap();

        for dir in SessionReader::list_sessions(env.path()).unwrap() {
            std::fs::remove_dir_all(dir).unwrap();
        }
        let report = index.refresh().unwrap();
        assert_eq!(
            report.removed, 0,
            "the lease outranks the missing directory"
        );
        assert_eq!(index.sessions(10).unwrap().len(), 1);
    }

    #[test]
    fn removed_session_directory_is_swept_from_the_index() {
        let env = IsolatedWorkspace::new();
        write_session(env.path(), "keep me", "alpha", "beta");
        let mut index = open_index(env.path());
        index.refresh().unwrap();
        assert_eq!(index.sessions(10).unwrap().len(), 1);

        for dir in SessionReader::list_sessions(env.path()).unwrap() {
            std::fs::remove_dir_all(dir).unwrap();
        }
        let report = index.refresh().unwrap();
        assert_eq!(report.removed, 1);
        assert!(index.sessions(10).unwrap().is_empty());
    }

    #[test]
    fn deleting_the_database_rebuilds_identical_results() {
        let env = IsolatedWorkspace::new();
        write_session(env.path(), "first goal", "alpha needle", "beta");
        write_session(env.path(), "second goal", "gamma needle", "delta");

        let mut index = open_index(env.path());
        index.refresh().unwrap();
        let before = index.search_events("needle", 10, None).unwrap();
        let headers_before = index.sessions(10).unwrap();
        let path = index.path().to_path_buf();
        assert!(path.is_file());

        // The index is derived: throw the database away and rebuild it.
        drop(index);
        std::fs::remove_file(&path).unwrap();
        let mut rebuilt = open_index(env.path());
        rebuilt.refresh().unwrap();

        assert_eq!(rebuilt.search_events("needle", 10, None).unwrap(), before);
        assert_eq!(rebuilt.sessions(10).unwrap(), headers_before);
    }

    #[test]
    fn foreign_application_id_is_rebuilt_in_place() {
        let env = IsolatedWorkspace::new();
        write_session(env.path(), "goal", "recoverable", "ok");
        let path = {
            let mut index = open_index(env.path());
            index.refresh().unwrap();
            index.path().to_path_buf()
        };

        // Simulate a database written by another application/version.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA application_id = 1234; PRAGMA user_version = 99;")
                .unwrap();
        }

        let mut index = open_index(env.path());
        let stamp: i32 = index
            .conn
            .query_row("PRAGMA application_id", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stamp, INDEX_APPLICATION_ID);
        // Tables were dropped, so the first refresh re-indexes from scratch.
        assert_eq!(index.refresh().unwrap().indexed, 1);
        assert_eq!(
            index.search_events("recoverable", 10, None).unwrap().len(),
            1
        );
    }

    #[test]
    fn transcript_is_parsed_once_per_revision() {
        let env = IsolatedWorkspace::new();
        let session_id = write_session(env.path(), "goal", "hello", "world");
        let mut index = open_index(env.path());
        index.refresh().unwrap();

        let first = index.transcript(&session_id).unwrap();
        assert_eq!(index.cold_cache_len(), 1);
        let second = index.transcript(&session_id).unwrap();
        assert!(Arc::ptr_eq(&first, &second), "unchanged log must be cached");
    }

    #[test]
    fn transcript_of_unknown_session_is_not_found() {
        let env = IsolatedWorkspace::new();
        let mut index = open_index(env.path());
        let err = index.transcript("missing").unwrap_err();
        assert!(matches!(err, Error::NotFound(_)));
    }

    #[test]
    fn leasing_pins_the_parsed_transcript_against_lru_pressure() {
        let env = IsolatedWorkspace::new();
        let mut ids = Vec::new();
        for i in 0..(DEFAULT_COLD_CACHE_CAPACITY + 1) {
            ids.push(write_session(
                env.path(),
                &format!("goal {i}"),
                "body",
                "reply",
            ));
        }
        let mut index = open_index(env.path());
        index.refresh().unwrap();

        index.lease(&ids[0]).unwrap();
        let leased = index.transcript(&ids[0]).unwrap();
        // Parsing every other session pushes the cache past its capacity.
        for id in &ids[1..] {
            index.transcript(id).unwrap();
        }
        assert_eq!(index.cold_cache_len(), DEFAULT_COLD_CACHE_CAPACITY);
        let again = index.transcript(&ids[0]).unwrap();
        assert!(
            Arc::ptr_eq(&leased, &again),
            "a leased transcript must never be evicted"
        );
    }

    #[test]
    fn search_results_flag_leased_sessions_as_active() {
        let env = IsolatedWorkspace::new();
        let session_id = write_session(env.path(), "goal", "active needle", "reply");
        let mut index = open_index(env.path());
        index.refresh().unwrap();

        let hits = index.search_events("needle", 10, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(!hits[0].active);

        index.lease(&session_id).unwrap();
        let hits = index.search_events("needle", 10, None).unwrap();
        assert!(hits[0].active, "a leased session's hits are marked active");
    }

    #[test]
    fn cold_cache_evicts_lru_but_never_a_leased_entry() {
        let mut cache: ColdReadCache<u8> = ColdReadCache::new(2);
        cache.put("a", "r", Arc::new(1));
        cache.put("b", "r", Arc::new(2));
        assert!(cache.lease("a"));
        // Inserting a third entry evicts the least recently used unleased one.
        let evicted = cache.put("c", "r", Arc::new(3));
        assert_eq!(evicted, vec!["b".to_string()]);
        assert_eq!(cache.len(), 2);
        assert!(cache.get("a", "r").is_some());
        assert!(cache.get("b", "r").is_none());

        cache.release("a");
        // "c" is now the least recently used *unleased* entry.
        let evicted = cache.put("d", "r", Arc::new(4));
        assert_eq!(evicted, vec!["c".to_string()]);
        // And "a" is next once it is no longer pinned.
        let evicted = cache.put("e", "r", Arc::new(5));
        assert_eq!(evicted, vec!["a".to_string()]);
        assert!(cache.get("a", "r").is_none());
    }

    #[test]
    fn cold_cache_returns_none_for_a_stale_revision() {
        let mut cache: ColdReadCache<u8> = ColdReadCache::new(2);
        cache.put("a", "rev-1", Arc::new(7));
        assert!(cache.get("a", "rev-2").is_none());
        assert!(cache.get("a", "rev-1").is_some());
    }

    #[test]
    fn leasing_an_absent_entry_is_a_noop() {
        let mut cache: ColdReadCache<u8> = ColdReadCache::new(2);
        assert!(!cache.lease("missing"));
        cache.put("a", "r", Arc::new(1));
        assert!(cache.lease("a"));
        cache.release("missing");
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn index_database_is_private() {
        let env = IsolatedWorkspace::new();
        let index = open_index(env.path());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(index.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert_eq!(index.path().file_name().unwrap(), INDEX_FILE_NAME);
        assert_eq!(index.workspace(), env.path());
    }

    #[test]
    fn empty_query_returns_no_hits() {
        let env = IsolatedWorkspace::new();
        write_session(env.path(), "goal", "hello", "world");
        let mut index = open_index(env.path());
        index.refresh().unwrap();
        assert!(index.search_events("   ", 10, None).unwrap().is_empty());
        assert!(index.search_sessions("", 10).unwrap().is_empty());
    }

    #[test]
    fn same_origin_reports_transcript_copies() {
        let env = IsolatedWorkspace::new();
        let source = write_session(env.path(), "origin", "hello", "world");
        // A byte-identical copy of the source log, as a fork would leave: same
        // first message uuid.
        let dirs = SessionReader::list_sessions(env.path()).unwrap();
        let source_dir = dirs
            .iter()
            .find(|d| d.file_name().unwrap().to_string_lossy() == source)
            .unwrap();
        let copy_id = format!("{source}-copy");
        let copy_dir = source_dir.parent().unwrap().join(&copy_id);
        std::fs::create_dir_all(&copy_dir).unwrap();
        std::fs::copy(
            source_dir.join(TRANSCRIPT_FILE),
            copy_dir.join(TRANSCRIPT_FILE),
        )
        .unwrap();
        let mut meta = SessionReader::load_meta(source_dir).unwrap();
        meta.session_id = copy_id.clone();
        std::fs::write(copy_dir.join(META_FILE), serde_json::to_vec(&meta).unwrap()).unwrap();

        let mut index = open_index(env.path());
        index.refresh().unwrap();
        assert_eq!(index.same_origin(&source).unwrap(), vec![copy_id]);
        assert!(index.same_origin("missing").unwrap().is_empty());
    }

    #[test]
    fn read_events_returns_content_in_order() {
        let env = IsolatedWorkspace::new();
        let session_id = write_session(env.path(), "goal", "hello", "world");
        let mut index = open_index(env.path());
        index.refresh().unwrap();

        let events = index.read_events(&session_id, 0, 10).unwrap();
        assert_eq!(events.len(), 3, "system + user + assistant");
        assert_eq!(events[0].index, 0);
        assert_eq!(events[1].content, "hello");
        assert_eq!(events[2].role, "assistant");
        assert_eq!(index.event_count(&session_id).unwrap(), 3);
        assert!(index.session(&session_id).unwrap().is_some());
        assert!(index.session("nope").unwrap().is_none());
        assert!(index.session_dir(&session_id).unwrap().is_some());
        assert!(index.session_dir("nope").unwrap().is_none());
    }

    #[test]
    fn assistant_tool_names_are_searchable() {
        let env = IsolatedWorkspace::new();
        let mut writer = SessionWriter::create(env.path(), "goal", "m", "p").unwrap();
        writer
            .append(&Message::user("go".to_string()), None, None)
            .unwrap();
        let call = crate::llm::ToolCall {
            id: "tc1".to_string(),
            name: "ReadFile".to_string(),
            arguments: serde_json::json!({}),
        };
        writer
            .append(
                &Message::assistant_with_tool_calls(String::new(), vec![call]),
                None,
                None,
            )
            .unwrap();
        writer.finish(SessionStatus::Completed).unwrap();

        let mut index = open_index(env.path());
        index.refresh().unwrap();
        let hits = index.search_events("ReadFile", 10, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].tool_name.as_deref(), Some("ReadFile"));
    }

    #[test]
    fn snippet_windows_around_the_match() {
        let content = format!("{}needle{}", "a".repeat(400), "b".repeat(400));
        let out = snippet(&content, &["needle".to_string()], 40);
        assert!(out.starts_with('…'));
        assert!(out.ends_with('…'));
        assert!(out.contains("needle"));
        assert!(out.chars().count() < content.chars().count());
    }

    #[test]
    fn snippet_keeps_short_content_intact() {
        let out = snippet("short body", &["short".to_string()], 40);
        assert_eq!(out, "short body");
    }

    #[test]
    fn like_pattern_escapes_wildcards() {
        assert_eq!(like_pattern("50%"), "%50\\%%");
        assert_eq!(like_pattern("a_b"), "%a\\_b%");
        assert_eq!(like_pattern("c\\d"), "%c\\\\d%");
    }

    #[test]
    fn match_expression_quotes_terms() {
        let terms = ["hello".to_string(), "a\"b".to_string()];
        let refs: Vec<&String> = terms.iter().collect();
        assert_eq!(match_expression(&refs), "\"hello\" \"a\"\"b\"");
    }
}
