//! Append-only, tamper-evident audit stream.
//!
//! Tool-level [`crate::tools::AuditMeta`] records *what* a tool did, but it
//! lives inside the transcript — a whole-file rewrite (`temp` + `rename`),
//! not an append-only log, with no caller identity, no authentication or
//! approval events, and no way to query "who did what, when". This module is
//! the enterprise audit trail that answers that question (issue #101).
//!
//! Design:
//!
//! - **Append-only.** Every event is a single JSON line appended with
//!   `O_APPEND`; the file is never truncated or rewritten. On Unix the file is
//!   created `0600`.
//! - **Tamper-evident.** Each record carries the BLAKE3 hash of its own
//!   contents plus the previous record's hash, forming a hash chain. Editing
//!   or reordering any historical line breaks verification ([`AuditLog::verify`]).
//! - **Attributed.** Every record carries an [`AuditActor`] (subject + tenant)
//!   so an action can be traced to a caller, not just a session.
//! - **Queryable.** [`AuditQuery`] filters by time range, actor, tenant and
//!   session for `/audit` retrieval and `/audit/export` (SIEM) endpoints.
//!
//! The process-wide handle ([`set_file_context`] / [`emit`]) mirrors
//! [`crate::notify`]'s process context: the HTTP server binds it once at
//! startup, and emission is best-effort — a failing audit write is logged and
//! swallowed so it can never take a request down.
//!
//! Known limits:
//!
//! - **No rotation.** The file only ever grows, so archiving is an operator
//!   task (stop the server, move the file aside, start again); retrieval
//!   defaults to the most recent records for the same reason.
//! - **Attribution is per session, not per channel.** A sub-agent worker runs
//!   its own runtime with no session id (see [`crate::multi`]), so a tool call
//!   it makes is recorded as [`AuditActor::local`] rather than as the caller
//!   who requested the delegation.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Genesis value for a record's `prev_hash`: 64 hex zeros, so the first
/// record's link is explicit rather than an empty string.
pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Subject used when an event has no attributable caller — e.g. a failed
/// authentication where the presented credential never resolved to an
/// identity.
pub const ANONYMOUS_SUBJECT: &str = "anonymous";

/// Who performed an audited action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditActor {
    /// Principal id — a verified subject (JWT `sub` / API-key subject).
    pub subject: String,
    /// Tenant the subject belongs to, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
}

impl AuditActor {
    /// An attributed actor.
    pub fn new(subject: impl Into<String>, tenant: Option<String>) -> Self {
        Self {
            subject: subject.into(),
            tenant,
        }
    }

    /// The actor for an event with no resolvable caller (failed auth, or a
    /// server-side action with no inbound identity).
    pub fn anonymous() -> Self {
        Self {
            subject: ANONYMOUS_SUBJECT.to_string(),
            tenant: None,
        }
    }

    /// The implicit local operator: the single user of a non-HTTP channel
    /// (CLI / REPL / TUI) where no request carried a credential.
    pub fn local() -> Self {
        Self {
            subject: "local".to_string(),
            tenant: None,
        }
    }
}

/// What happened. Tagged by `kind` in the serialized record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuditAction {
    /// A tool invocation reached dispatch and returned — including one the
    /// registry refused (`ok: false`). Carries the tool name, its side-effect
    /// class and whether it succeeded.
    ///
    /// Requests rejected *before* dispatch (plan mode, permission hook, hook
    /// skip/error in [`crate::run_core`]) never produce an event: they carry
    /// no [`crate::tools::AuditMeta`], so there is nothing to mirror.
    ToolCall {
        tool: String,
        side_effect: String,
        ok: bool,
    },
    /// A pending plan was approved or rejected by a caller.
    ApprovalDecision { decision: String },
    /// A management/administrative mutation (session delete/patch, …).
    AdminAction { action: String },
    /// A credential was presented and rejected.
    AuthFailure { reason: String },
}

/// One immutable line of the audit stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditRecord {
    /// Monotonic 0-based sequence number.
    pub seq: u64,
    /// Unix epoch milliseconds when the event was recorded.
    pub ts: i64,
    /// Caller the event is attributed to.
    pub actor: AuditActor,
    /// Session the action concerned, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// What happened.
    pub action: AuditAction,
    /// Hash of the previous record (`[GENESIS_HASH]` for the first).
    pub prev_hash: String,
    /// BLAKE3 of this record's contents (including `prev_hash`).
    pub hash: String,
}

/// Fixed-order serialization used to compute a record's chain hash. Field
/// order is what makes the digest stable across runs.
#[derive(Serialize)]
struct ChainMaterial<'a> {
    seq: u64,
    ts: i64,
    actor: &'a AuditActor,
    session: &'a Option<String>,
    action: &'a AuditAction,
    prev_hash: &'a str,
}

fn record_hash(
    seq: u64,
    ts: i64,
    actor: &AuditActor,
    session: &Option<String>,
    action: &AuditAction,
    prev_hash: &str,
) -> std::io::Result<String> {
    let material = ChainMaterial {
        seq,
        ts,
        actor,
        session,
        action,
        prev_hash,
    };
    let bytes = serde_json::to_vec(&material)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A break in the audit chain — a tampered, reordered or truncated line.
#[derive(Debug)]
pub enum AuditChainError {
    Io(std::io::Error),
    /// A line was not a valid [`AuditRecord`].
    Parse {
        line: usize,
        message: String,
    },
    /// A record's own hash did not match its recomputed contents.
    HashMismatch {
        line: usize,
        seq: u64,
    },
    /// A record did not link to the previous record's hash.
    PrevHashMismatch {
        line: usize,
        seq: u64,
    },
    /// Sequence numbers are not contiguous from 0.
    SeqMismatch {
        line: usize,
        expected: u64,
        found: u64,
    },
}

impl std::fmt::Display for AuditChainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "audit log io error: {e}"),
            Self::Parse { line, message } => {
                write!(f, "audit log line {line}: unparsable record: {message}")
            }
            Self::HashMismatch { line, seq } => {
                write!(
                    f,
                    "audit log line {line}: record {seq} hash mismatch (tampered)"
                )
            }
            Self::PrevHashMismatch { line, seq } => {
                write!(
                    f,
                    "audit log line {line}: record {seq} does not link to its predecessor"
                )
            }
            Self::SeqMismatch {
                line,
                expected,
                found,
            } => write!(
                f,
                "audit log line {line}: expected seq {expected}, found {found}"
            ),
        }
    }
}

impl std::error::Error for AuditChainError {}

impl From<std::io::Error> for AuditChainError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl AuditChainError {
    /// Human-readable one-liner for HTTP error bodies.
    pub fn message(&self) -> String {
        self.to_string()
    }
}

/// Filters for retrieving records from the stream.
#[derive(Debug, Clone, Default)]
pub struct AuditQuery {
    /// Inclusive lower bound on `ts` (epoch millis).
    pub from_ts: Option<i64>,
    /// Inclusive upper bound on `ts` (epoch millis).
    pub to_ts: Option<i64>,
    /// Exact actor subject.
    pub actor: Option<String>,
    /// Exact actor tenant.
    pub tenant: Option<String>,
    /// Exact session id.
    pub session: Option<String>,
    /// Cap on the number of returned records (most recent N, in order).
    pub limit: Option<usize>,
}

impl AuditQuery {
    /// Whether `record` satisfies every configured filter.
    pub fn matches(&self, record: &AuditRecord) -> bool {
        if let Some(from) = self.from_ts {
            if record.ts < from {
                return false;
            }
        }
        if let Some(to) = self.to_ts {
            if record.ts > to {
                return false;
            }
        }
        if let Some(actor) = &self.actor {
            if &record.actor.subject != actor {
                return false;
            }
        }
        if let Some(tenant) = &self.tenant {
            if record.actor.tenant.as_deref() != Some(tenant.as_str()) {
                return false;
            }
        }
        if let Some(session) = &self.session {
            if record.session.as_deref() != Some(session.as_str()) {
                return false;
            }
        }
        true
    }

    /// Apply the filter, preserving chronological order and keeping the most
    /// recent `limit` records when one is set.
    pub fn apply(&self, records: Vec<AuditRecord>) -> Vec<AuditRecord> {
        let mut kept: Vec<AuditRecord> = records.into_iter().filter(|r| self.matches(r)).collect();
        if let Some(limit) = self.limit {
            if kept.len() > limit {
                kept.drain(0..kept.len() - limit);
            }
        }
        kept
    }
}

/// Handle over one audit file. Holds the chain head so appends are `O(1)`;
/// [`AuditLog::open`] recovers it by scanning the existing file.
#[derive(Debug, Clone)]
pub struct AuditLog {
    path: PathBuf,
    next_seq: u64,
    last_hash: String,
}

impl AuditLog {
    /// Open (or create) the log at `path`, recovering the chain head.
    pub fn open(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let (next_seq, last_hash) = read_tail(&path)?;
        Ok(Self {
            path,
            next_seq,
            last_hash,
        })
    }

    /// The file backing this log.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The sequence number the next append will use.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Append one event, extending the hash chain. Returns the record written.
    pub fn append(
        &mut self,
        actor: AuditActor,
        session: Option<String>,
        action: AuditAction,
    ) -> std::io::Result<AuditRecord> {
        let seq = self.next_seq;
        let ts = now_millis();
        let prev_hash = self.last_hash.clone();
        let hash = record_hash(seq, ts, &actor, &session, &action, &prev_hash)?;
        let record = AuditRecord {
            seq,
            ts,
            actor,
            session,
            action,
            prev_hash,
            hash: hash.clone(),
        };
        let mut line = serde_json::to_string(&record)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        append_line(&self.path, line.as_bytes())?;
        self.next_seq = seq + 1;
        self.last_hash = hash;
        Ok(record)
    }

    /// Read every record back. Empty when the file does not exist yet.
    pub fn records(&self) -> std::io::Result<Vec<AuditRecord>> {
        read_records(&self.path)
    }

    /// Recompute the whole chain and report the number of verified records.
    pub fn verify(&self) -> Result<usize, AuditChainError> {
        verify_chain(&self.path)
    }
}

fn append_line(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(bytes)?;
    file.flush()
}

fn read_tail(path: &Path) -> std::io::Result<(u64, String)> {
    let records = read_records(path)?;
    match records.last() {
        Some(last) => Ok((last.seq + 1, last.hash.clone())),
        None => Ok((0, GENESIS_HASH.to_string())),
    }
}

fn read_records(path: &Path) -> std::io::Result<Vec<AuditRecord>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let record: AuditRecord = serde_json::from_str(line)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        out.push(record);
    }
    Ok(out)
}

/// Recompute the chain over `path`, returning the count of verified records.
///
/// The first record must link to [`GENESIS_HASH`]; every subsequent record
/// must link to its predecessor's hash, carry a contiguous `seq`, and hash to
/// exactly its recomputed digest.
pub fn verify_chain(path: &Path) -> Result<usize, AuditChainError> {
    // Parsed line by line rather than through `read_records` so that a line
    // that is not a record at all is reported as [`AuditChainError::Parse`]
    // (a chain verdict) instead of a generic I/O error.
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(AuditChainError::Io(e)),
    };
    let mut expected_prev = GENESIS_HASH.to_string();
    let mut verified: u64 = 0;
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let line_no = i + 1;
        let record: AuditRecord =
            serde_json::from_str(line).map_err(|e| AuditChainError::Parse {
                line: line_no,
                message: e.to_string(),
            })?;
        if record.seq != verified {
            return Err(AuditChainError::SeqMismatch {
                line: line_no,
                expected: verified,
                found: record.seq,
            });
        }
        if record.prev_hash != expected_prev {
            return Err(AuditChainError::PrevHashMismatch {
                line: line_no,
                seq: record.seq,
            });
        }
        let recomputed = record_hash(
            record.seq,
            record.ts,
            &record.actor,
            &record.session,
            &record.action,
            &record.prev_hash,
        )
        .map_err(AuditChainError::Io)?;
        if recomputed != record.hash {
            return Err(AuditChainError::HashMismatch {
                line: line_no,
                seq: record.seq,
            });
        }
        expected_prev = record.hash.clone();
        verified += 1;
    }
    Ok(verified as usize)
}

// ---------------------------------------------------------------------------
// Process-wide context
// ---------------------------------------------------------------------------

static GLOBAL: Mutex<Option<AuditLog>> = Mutex::new(None);

fn global_cell() -> &'static Mutex<Option<AuditLog>> {
    &GLOBAL
}

/// Bind the process audit log under `workspace`'s user data dir
/// (`<user_workspace_dir>/audit/audit.jsonl`). Called once by the HTTP server
/// at startup.
pub fn set_file_context(workspace: &Path) {
    let root = crate::paths::user_workspace_dir(workspace)
        .unwrap_or_else(|_| workspace.join(".recursive"));
    let path = root.join("audit").join("audit.jsonl");
    if let Err(e) = set_log_path(path.clone()) {
        // Opening scans the tail to recover the chain head, so an unreadable
        // or truncated file disables auditing for the whole process. Say so
        // loudly: a silently unaudited server is the failure mode this feature
        // exists to prevent.
        tracing::error!(
            error = %e,
            path = %path.display(),
            "audit: failed to open audit log — auditing is DISABLED for this \
             process until the file is repaired or moved aside"
        );
    }
}

/// Bind the process audit log to an explicit path (tests, custom deployments).
pub fn set_log_path(path: impl Into<PathBuf>) -> std::io::Result<()> {
    let log = AuditLog::open(path)?;
    *global_cell().lock().unwrap_or_else(|e| e.into_inner()) = Some(log);
    Ok(())
}

/// Path of the bound audit log, or `None` when none is bound.
pub fn log_path() -> Option<PathBuf> {
    global_cell()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|l| l.path().to_path_buf())
}

/// Whether a process audit log is bound.
pub fn is_enabled() -> bool {
    global_cell()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
}

/// Unbind the process audit log (test teardown).
pub fn clear() {
    *global_cell().lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Append `action` to the bound audit log, attributed to `actor`.
///
/// Best-effort: when no log is bound the event is dropped silently, and a
/// write failure is logged (never propagated) — auditing must not be able to
/// fail the operation it observes.
pub fn emit(actor: AuditActor, session: Option<String>, action: AuditAction) {
    let mut guard = global_cell().lock().unwrap_or_else(|e| e.into_inner());
    let Some(log) = guard.as_mut() else {
        return;
    };
    if let Err(e) = log.append(actor, session, action) {
        tracing::warn!(error = %e, "audit: failed to append event");
    }
}

/// Read every record from the bound audit log (empty when none is bound).
pub fn records() -> std::io::Result<Vec<AuditRecord>> {
    match global_cell()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
    {
        Some(log) => log.records(),
        None => Ok(Vec::new()),
    }
}

/// Verify the bound audit log's chain (0 records when none is bound).
pub fn verify() -> Result<usize, AuditChainError> {
    match global_cell()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
    {
        Some(log) => log.verify(),
        None => Ok(0),
    }
}

// ---------------------------------------------------------------------------
// Session → actor attribution
// ---------------------------------------------------------------------------

/// Cap on the session→actor registry. Registration is driven by inbound
/// requests, so the table is bounded FIFO — without a cap any credential
/// holder could grow it with arbitrary session ids.
const MAX_TRACKED_SESSIONS: usize = 1024;

/// Session → caller attributions, in insertion order so the oldest entry can
/// be evicted once the cap is reached.
#[derive(Default)]
struct SessionActors {
    actors: HashMap<String, AuditActor>,
    order: VecDeque<String>,
}

impl SessionActors {
    fn register(&mut self, session: &str, actor: AuditActor) {
        if !self.actors.contains_key(session) {
            while self.order.len() >= MAX_TRACKED_SESSIONS {
                match self.order.pop_front() {
                    Some(oldest) => {
                        self.actors.remove(&oldest);
                    }
                    None => break,
                }
            }
            self.order.push_back(session.to_string());
        }
        self.actors.insert(session.to_string(), actor);
    }

    fn forget(&mut self, session: &str) {
        self.actors.remove(session);
        self.order.retain(|tracked| tracked != session);
    }
}

static SESSION_ACTORS: Mutex<Option<SessionActors>> = Mutex::new(None);

fn session_actors_cell() -> &'static Mutex<Option<SessionActors>> {
    &SESSION_ACTORS
}

/// Remember the caller behind `session` so in-process tool dispatch — which
/// runs inside the runtime, without the originating request in scope — can
/// attribute its events. The HTTP audit middleware calls this for every
/// identity-bearing *mutating* `/sessions/{id}*` request.
pub fn register_session_actor(session: &str, actor: AuditActor) {
    let mut guard = session_actors_cell()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    guard
        .get_or_insert_with(SessionActors::default)
        .register(session, actor);
}

/// The caller recorded for `session`, if any.
pub fn session_actor(session: &str) -> Option<AuditActor> {
    session_actors_cell()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|tracked| tracked.actors.get(session).cloned())
}

/// Drop the recorded caller for `session` (on session deletion, or to undo a
/// registration made for a request the handler ended up refusing).
pub fn forget_session_actor(session: &str) {
    let mut guard = session_actors_cell()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(tracked) = guard.as_mut() {
        tracked.forget(session);
    }
}

/// The actor for a tool run in `session`: the registered caller, or the
/// implicit local operator when the session was never seen on an
/// identity-bearing request (CLI / REPL / TUI).
pub fn resolve_session_actor(session: Option<&str>) -> AuditActor {
    session
        .and_then(session_actor)
        .unwrap_or_else(AuditActor::local)
}

/// Process-wide lock shared by every test that binds, clears or emits into
/// the audit log, so those tests cannot race each other across modules.
#[cfg(test)]
pub(crate) fn test_lock() -> &'static Mutex<()> {
    static TEST_LOCK: Mutex<()> = Mutex::new(());
    &TEST_LOCK
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor(subject: &str) -> AuditActor {
        AuditActor::new(subject, None)
    }

    fn tool(tool: &str) -> AuditAction {
        AuditAction::ToolCall {
            tool: tool.to_string(),
            side_effect: "mutating".to_string(),
            ok: true,
        }
    }

    #[test]
    fn append_assigns_contiguous_seq_and_links_chain() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("open");

        let first = log
            .append(actor("alice"), Some("s1".into()), tool("Write"))
            .expect("append");
        let second = log
            .append(actor("bob"), Some("s1".into()), tool("Bash"))
            .expect("append");

        assert_eq!(first.seq, 0);
        assert_eq!(first.prev_hash, GENESIS_HASH);
        assert_eq!(second.seq, 1);
        assert_eq!(second.prev_hash, first.hash);
        assert_ne!(first.hash, second.hash, "hashes must differ");
        assert_eq!(log.next_seq(), 2);
    }

    #[test]
    fn reopen_recovers_chain_head() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        {
            let mut log = AuditLog::open(&path).expect("open");
            log.append(actor("alice"), None, tool("Read"))
                .expect("append");
        }
        let mut reopened = AuditLog::open(&path).expect("reopen");
        let next = reopened
            .append(actor("alice"), None, tool("Read"))
            .expect("append");
        assert_eq!(next.seq, 1, "seq must continue after reopen");
        assert_eq!(reopened.verify().expect("verify"), 2);
    }

    #[test]
    fn verify_accepts_untouched_chain() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("open");
        for i in 0..3 {
            log.append(actor("alice"), None, tool(&format!("t{i}")))
                .expect("append");
        }
        assert_eq!(log.verify().expect("verify"), 3);
    }

    #[test]
    fn verify_detects_content_tampering() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("open");
        log.append(actor("alice"), None, tool("Read"))
            .expect("append");
        log.append(actor("alice"), None, tool("Write"))
            .expect("append");

        // Rewrite the first line's action while keeping its (now stale) hash.
        let text = std::fs::read_to_string(&path).expect("read");
        let tampered = text.replacen("\"Read\"", "\"Bash\"", 1);
        std::fs::write(&path, tampered).expect("write");

        assert!(matches!(
            verify_chain(&path),
            Err(AuditChainError::HashMismatch { .. })
        ));
    }

    #[test]
    fn verify_detects_dropped_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("open");
        log.append(actor("a"), None, tool("Read")).expect("append");
        log.append(actor("a"), None, tool("Write")).expect("append");

        // Keep only the last line: it no longer links to genesis.
        let text = std::fs::read_to_string(&path).expect("read");
        let last = text.lines().nth(1).expect("second line");
        std::fs::write(&path, format!("{last}\n")).expect("write");

        assert!(matches!(
            verify_chain(&path),
            Err(AuditChainError::SeqMismatch { .. })
                | Err(AuditChainError::PrevHashMismatch { .. })
        ));
    }

    #[test]
    fn verify_empty_log_is_zero() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        assert_eq!(verify_chain(&path).expect("verify"), 0);
    }

    #[test]
    fn verify_reports_unparsable_line_as_chain_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("open");
        log.append(actor("alice"), None, tool("Read"))
            .expect("append");
        let mut text = std::fs::read_to_string(&path).expect("read");
        text.push_str("not a record\n");
        std::fs::write(&path, text).expect("write");

        match verify_chain(&path) {
            Err(e @ AuditChainError::Parse { line, .. }) => {
                assert_eq!(line, 2, "the physical line number is reported");
                assert!(
                    e.message().contains("unparsable"),
                    "the verdict must explain itself, got '{}'",
                    e.message()
                );
            }
            other => panic!("expected a parse failure, got {other:?}"),
        }
    }

    #[test]
    fn query_filters_by_actor_tenant_session_and_time() {
        let mk =
            |seq: u64, ts: i64, subject: &str, tenant: Option<&str>, session: &str| AuditRecord {
                seq,
                ts,
                actor: AuditActor::new(subject, tenant.map(str::to_string)),
                session: Some(session.to_string()),
                action: tool("Read"),
                prev_hash: GENESIS_HASH.to_string(),
                hash: format!("h{seq}"),
            };
        let records = vec![
            mk(0, 100, "alice", Some("acme"), "s1"),
            mk(1, 200, "bob", None, "s2"),
            mk(2, 300, "alice", Some("acme"), "s2"),
        ];

        assert_eq!(
            AuditQuery {
                actor: Some("alice".into()),
                ..Default::default()
            }
            .apply(records.clone())
            .len(),
            2
        );
        assert_eq!(
            AuditQuery {
                tenant: Some("acme".into()),
                session: Some("s2".into()),
                ..Default::default()
            }
            .apply(records.clone())
            .len(),
            1
        );
        assert_eq!(
            AuditQuery {
                from_ts: Some(150),
                to_ts: Some(250),
                ..Default::default()
            }
            .apply(records.clone())
            .len(),
            1
        );
        // limit keeps the most recent N, in chronological order.
        let limited = AuditQuery {
            limit: Some(2),
            ..Default::default()
        }
        .apply(records.clone());
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[0].seq, 1);
        assert_eq!(limited[1].seq, 2);
    }

    #[test]
    fn record_serde_round_trips() {
        let record = AuditRecord {
            seq: 7,
            ts: 1700,
            actor: AuditActor::new("alice", Some("acme".into())),
            session: Some("s1".into()),
            action: AuditAction::ApprovalDecision {
                decision: "approved".into(),
            },
            prev_hash: GENESIS_HASH.to_string(),
            hash: "abc".into(),
        };
        let json = serde_json::to_string(&record).expect("serialize");
        let back: AuditRecord = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, record);
        assert!(json.contains("\"kind\":\"approval_decision\""));
    }

    #[test]
    fn global_emit_is_noop_without_context() {
        let _guard = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        clear();
        // No panic, no error — the event is simply dropped.
        emit(actor("alice"), None, tool("Read"));
        assert!(!is_enabled());
        assert_eq!(records().expect("records").len(), 0);
    }

    #[test]
    fn global_emit_appends_and_is_queryable() {
        let _guard = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        set_log_path(&path).expect("bind");
        assert!(is_enabled());

        emit(actor("alice"), Some("s1".into()), tool("Write"));
        emit(
            AuditActor::anonymous(),
            None,
            AuditAction::AuthFailure {
                reason: "bad key".into(),
            },
        );

        // Other tests running in parallel may emit into the bound log (their
        // events are unrelated and use different sessions/actors), so match on
        // our own records rather than asserting an exact total.
        let records = records().expect("records");
        assert!(records
            .iter()
            .any(|r| { r.session.as_deref() == Some("s1") && r.actor.subject == "alice" }));
        assert!(records.iter().any(|r| matches!(
            &r.action,
            AuditAction::AuthFailure { reason } if reason == "bad key"
        )));
        assert!(verify().expect("verify") >= 2);
        assert_eq!(log_path().as_deref(), Some(path.as_path()));

        clear();
        assert!(!is_enabled());
    }

    #[test]
    fn session_actor_registry_round_trips() {
        let _guard = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        register_session_actor("sess-a", AuditActor::new("alice", Some("acme".into())));
        assert_eq!(
            session_actor("sess-a"),
            Some(AuditActor::new("alice", Some("acme".into())))
        );
        // Unknown session falls back to the implicit local operator.
        assert_eq!(resolve_session_actor(None), AuditActor::local());
        assert_eq!(resolve_session_actor(Some("missing")), AuditActor::local());
        assert_eq!(
            resolve_session_actor(Some("sess-a")).subject,
            "alice",
            "a registered session resolves to its caller"
        );
        forget_session_actor("sess-a");
        assert!(session_actor("sess-a").is_none());
    }

    #[test]
    fn session_registry_evicts_the_oldest_once_capped() {
        let mut tracked = SessionActors::default();
        let total = MAX_TRACKED_SESSIONS + 3;
        for i in 0..total {
            tracked.register(&format!("s{i}"), actor("alice"));
        }
        assert_eq!(tracked.actors.len(), MAX_TRACKED_SESSIONS);
        assert_eq!(tracked.order.len(), MAX_TRACKED_SESSIONS);
        assert!(!tracked.actors.contains_key("s0"), "oldest evicted");
        assert!(tracked.actors.contains_key(&format!("s{}", total - 1)));

        // Re-registering a known session refreshes its actor without changing
        // its place in the eviction order.
        tracked.register("s100", actor("bob"));
        assert_eq!(tracked.order.front().map(String::as_str), Some("s3"));
        assert_eq!(
            tracked.actors.get("s100").map(|a| a.subject.as_str()),
            Some("bob")
        );
    }

    #[test]
    fn session_registry_forget_releases_the_slot() {
        let mut tracked = SessionActors::default();
        tracked.register("a", actor("alice"));
        tracked.register("b", actor("bob"));

        tracked.forget("a");

        assert!(!tracked.actors.contains_key("a"));
        assert_eq!(tracked.order.len(), 1, "order must not leak dead ids");
        assert!(tracked.actors.contains_key("b"));
    }
}
