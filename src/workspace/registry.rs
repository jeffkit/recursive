//! Multi-tenant workspace registry (issue #135).
//!
//! A workspace (a.k.a. project) is the organisational unit above a session:
//! it groups the sessions that share a directory, carries an archive lifecycle,
//! and answers "what is still running here" before it is archived. The registry
//! that records them is a *header-only* index — it stores a canonical-path key,
//! a display name, and a small header per session (id, status, timestamps,
//! prompts). It never reads a transcript body, so listing workspaces and their
//! sessions stays cheap no matter how large the histories grow.
//!
//! Three properties are load-bearing and each has an acceptance test:
//!
//! 1. **realpath canon.** The registry key is the `realpath` of the directory
//!    (via [`std::fs::canonicalize`], which resolves symlinks like `fs.realpath`).
//!    Registering a directory that resolves to one already registered — however
//!    it is spelled, symlink or not — is rejected with
//!    [`Error::WorkspaceConflict`]. Two entries can therefore never alias one
//!    directory.
//! 2. **Two-write mutations with crash recovery.** Every mutation writes a
//!    *pending marker* first, then rewrites the index, then clears the marker.
//!    [`WorkspaceRegistry::recover`] (run from [`WorkspaceRegistry::open`])
//!    finishes any interrupted mutation on the next start. A state that is
//!    inconsistent *without* a marker is not guessed at: it fails loud with
//!    [`Error::WorkspaceCorrupt`].
//! 3. **Archiving is an admission gate.** [`WorkspaceRegistry::archive`] asks an
//!    [`ActivityProbe`](crate::workspace::ActivityProbe) what is still running
//!    and either refuses (admission) or, with
//!    [`ArchivePolicy::StopThenArchive`], persists the archive write *before*
//!    issuing the stop — the archive gate precedes the work it would wake.
//!    Removing a workspace, and archiving it, never touch the directory or the
//!    session history on disk.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::session::{SessionMeta, SessionReader, SessionStatus};
use crate::workspace::activity::{ActiveWork, ActivityProbe};

/// Schema version written into `index.json`. Bump on a
/// non-backward-compatible change to the on-disk registry shape.
const REGISTRY_SCHEMA_VERSION: u32 = 1;

/// Header-only index entry for one session inside a workspace.
///
/// Built solely from `.meta.json`; the transcript body is never read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHeader {
    pub session_id: String,
    pub status: SessionStatus,
    pub updated_at: String,
    pub goal: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prompt: Option<String>,
}

impl From<&SessionMeta> for SessionHeader {
    fn from(m: &SessionMeta) -> Self {
        Self {
            session_id: m.session_id.clone(),
            status: m.status,
            updated_at: m.updated_at.clone(),
            goal: m.goal.clone(),
            name: m.name.clone(),
            last_prompt: m.last_prompt.clone(),
        }
    }
}

/// One registered workspace. `id` is the realpath canon (the uniqueness key);
/// `display` is the path as the caller spelled it, kept only so the registry can
/// show a user what they typed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRecord {
    /// Canonical (realpath-resolved, absolute) directory. Uniqueness key.
    pub id: String,
    /// The path as originally registered (may be a symlink or a `..`-laden
    /// spelling). Display only — never used for identity.
    pub display: String,
    /// Human-readable name; defaults to the directory's base name.
    pub name: String,
    pub created_at: String,
    /// Archive lifecycle flag. Archived workspaces are retained in full; the
    /// flag only marks them inactive so they can be filtered out of listings.
    pub archived: bool,
    /// Header-only index of this workspace's sessions. Refreshed explicitly via
    /// [`WorkspaceRegistry::index_sessions`].
    #[serde(default)]
    pub sessions: Vec<SessionHeader>,
}

/// What an archive call did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveOutcome {
    pub workspace: String,
    /// Active work observed *before* the archive write.
    pub active_work: Vec<ActiveWork>,
    /// The subset of `active_work` the probe actually stopped. Work the probe
    /// could not reach (a live turn's stop handle lives with the runtime) stays
    /// in `active_work` only, so a caller never claims a stop that did not
    /// happen.
    pub stopped: Vec<ActiveWork>,
}

/// How to treat active work while archiving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchivePolicy {
    /// Refuse to archive while anything is running (default).
    Admit,
    /// Persist the archive write first, then ask the work to stop.
    StopThenArchive,
}

/// Summary of a [`WorkspaceRegistry::recover`] pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Pending mutations that still had work to do and were applied.
    pub applied: usize,
    /// Pending mutations whose effect was already present (no-op).
    pub already_applied: usize,
    /// Markers cleared after their effect was made durable.
    pub cleared: usize,
}

impl RecoveryReport {
    pub fn is_clean(&self) -> bool {
        self.applied == 0 && self.already_applied == 0 && self.cleared == 0
    }
}

/// The on-disk registry document.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RegistryIndex {
    #[serde(default = "default_schema_version")]
    version: u32,
    #[serde(default)]
    workspaces: Vec<WorkspaceRecord>,
}

fn default_schema_version() -> u32 {
    REGISTRY_SCHEMA_VERSION
}

impl Default for RegistryIndex {
    fn default() -> Self {
        Self {
            version: REGISTRY_SCHEMA_VERSION,
            workspaces: Vec::new(),
        }
    }
}

/// A mutation journaled to disk before the index is rewritten, so a crash
/// between the two writes is repairable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum PendingMutation {
    Create { record: WorkspaceRecord },
    Delete { id: String },
    Archive { id: String, archived: bool },
}

impl PendingMutation {
    /// The registry key this mutation targets (used for the marker filename).
    fn key(&self) -> &str {
        match self {
            PendingMutation::Create { record } => &record.id,
            PendingMutation::Delete { id } => id,
            PendingMutation::Archive { id, .. } => id,
        }
    }
}

/// The registry: its root directory plus the operation chain that serialises
/// every mutation. Reads do not take the chain (so a probe invoked from inside
/// an archive can still read state), mutations always do.
pub struct WorkspaceRegistry {
    root: PathBuf,
    chain: Mutex<()>,
}

impl WorkspaceRegistry {
    /// Open the per-user registry (`<user_data_dir>/workspace-registry/`),
    /// recovering any interrupted mutation and validating the result.
    ///
    /// A corrupt registry fails loud here — the point of running recovery on
    /// startup is that a broken state is surfaced, never silently absorbed.
    pub fn open() -> Result<Self> {
        Self::at(crate::paths::user_data_dir().join("workspace-registry"))
    }

    /// Open a registry rooted at an explicit directory (tests, tools).
    pub fn at(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(root.join("pending")).map_err(Error::Io)?;
        let registry = Self {
            root,
            chain: Mutex::new(()),
        };
        registry.recover()?;
        registry.validate()?;
        Ok(registry)
    }

    /// Root directory of this registry.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn index_path(&self) -> PathBuf {
        self.root.join("index.json")
    }

    fn pending_dir(&self) -> PathBuf {
        self.root.join("pending")
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.chain.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn marker_path(&self, key: &str) -> PathBuf {
        let hash = blake3::hash(key.as_bytes());
        let name: String = hash.to_hex().chars().take(16).collect();
        self.pending_dir().join(format!("{name}.json"))
    }

    fn load_index(&self) -> Result<RegistryIndex> {
        let path = self.index_path();
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(RegistryIndex::default())
            }
            Err(e) => return Err(Error::Io(e)),
        };
        let index: RegistryIndex =
            serde_json::from_slice(&bytes).map_err(|e| Error::WorkspaceCorrupt {
                message: format!("cannot parse {}: {e}", path.display()),
            })?;
        // A registry written by a newer build may carry fields this one would
        // silently drop — refuse it the way `SessionReader::load_meta` refuses
        // a newer session schema.
        if index.version > REGISTRY_SCHEMA_VERSION {
            return Err(Error::WorkspaceCorrupt {
                message: format!(
                    "{} has schema version {} (this build supports {REGISTRY_SCHEMA_VERSION})",
                    path.display(),
                    index.version
                ),
            });
        }
        Ok(index)
    }

    fn save_index(&self, index: &RegistryIndex) -> Result<()> {
        let json = serde_json::to_string_pretty(index).map_err(|e| Error::WorkspaceCorrupt {
            message: format!("cannot serialize registry: {e}"),
        })?;
        crate::atomic::atomic_write(&self.index_path(), json.as_bytes()).map_err(Error::Io)
    }

    fn write_marker(&self, mutation: &PendingMutation) -> Result<()> {
        let json = serde_json::to_string_pretty(mutation).map_err(|e| Error::WorkspaceCorrupt {
            message: format!("cannot serialize pending marker: {e}"),
        })?;
        crate::atomic::atomic_write(&self.marker_path(mutation.key()), json.as_bytes())
            .map_err(Error::Io)
    }

    fn clear_marker(&self, key: &str) -> Result<()> {
        match std::fs::remove_file(self.marker_path(key)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Io(e)),
        }
    }

    /// All registered workspaces, sorted by name then canonical id.
    /// Archived entries are included.
    pub fn list(&self) -> Result<Vec<WorkspaceRecord>> {
        Ok(self.load_index()?.workspaces)
    }

    /// Look up one record by canonical id, name, or a path that resolves to it.
    pub fn get(&self, key: &str) -> Result<WorkspaceRecord> {
        let index = self.load_index()?;
        let id = resolve(&index, key)?;
        index
            .workspaces
            .into_iter()
            .find(|w| w.id == id)
            .ok_or_else(|| Error::NotFound(format!("workspace `{key}`")))
    }

    /// Register `path` as a workspace. `name` defaults to the directory's base
    /// name. Rejects a directory whose realpath is already registered.
    pub fn create(&self, path: &Path, name: Option<String>) -> Result<WorkspaceRecord> {
        let canonical = realpath(path)?;
        let id = canonical.to_string_lossy().into_owned();
        let name = name.unwrap_or_else(|| default_name(&canonical));
        let _guard = self.lock();

        let mut index = self.load_index()?;
        if let Some(existing) = index.workspaces.iter().find(|w| w.id == id) {
            return Err(Error::WorkspaceConflict {
                canonical: id,
                existing: existing.name.clone(),
            });
        }

        let record = WorkspaceRecord {
            id,
            display: path.to_string_lossy().into_owned(),
            name,
            created_at: crate::session::chrono_lite_now(),
            archived: false,
            sessions: Vec::new(),
        };

        self.write_marker(&PendingMutation::Create {
            record: record.clone(),
        })?;
        index.workspaces.push(record.clone());
        sort(&mut index.workspaces);
        self.save_index(&index)?;
        self.clear_marker(&record.id)?;
        Ok(record)
    }

    /// Remove a workspace from the registry. **Non-destructive**: the directory,
    /// its sessions, and its history are left untouched on disk.
    pub fn remove(&self, key: &str) -> Result<WorkspaceRecord> {
        let _guard = self.lock();
        let mut index = self.load_index()?;
        let id = resolve(&index, key)?;
        let removed = match index.workspaces.iter().position(|w| w.id == id) {
            Some(pos) => index.workspaces.remove(pos),
            None => return Err(Error::NotFound(format!("workspace `{key}`"))),
        };

        self.write_marker(&PendingMutation::Delete {
            id: removed.id.clone(),
        })?;
        self.save_index(&index)?;
        self.clear_marker(&removed.id)?;
        Ok(removed)
    }

    /// Set the archive flag on a workspace without the admission gate. Use
    /// [`Self::archive`] when the "is anything running?" check matters.
    pub fn set_archived(&self, key: &str, archived: bool) -> Result<WorkspaceRecord> {
        let _guard = self.lock();
        let mut index = self.load_index()?;
        let id = resolve(&index, key)?;
        self.write_marker(&PendingMutation::Archive {
            id: id.clone(),
            archived,
        })?;
        let record = index
            .workspaces
            .iter_mut()
            .find(|w| w.id == id)
            .ok_or_else(|| Error::NotFound(format!("workspace `{key}`")))?;
        record.archived = archived;
        let snapshot = record.clone();
        self.save_index(&index)?;
        self.clear_marker(&id)?;
        Ok(snapshot)
    }

    /// Whether a workspace is currently archived.
    pub fn is_archived(&self, key: &str) -> Result<bool> {
        Ok(self.get(key)?.archived)
    }

    /// Archive a workspace under the admission gate.
    ///
    /// The probe is asked what is running. With [`ArchivePolicy::Admit`] any
    /// active work blocks the archive ([`Error::WorkspaceActiveWork`]). With
    /// [`ArchivePolicy::StopThenArchive`] the archive write is persisted first
    /// and only then is `probe.stop` called — so a crash between the two leaves
    /// the workspace archived (never "stopped but not archived").
    pub fn archive(
        &self,
        key: &str,
        probe: &dyn ActivityProbe,
        policy: ArchivePolicy,
    ) -> Result<ArchiveOutcome> {
        let _guard = self.lock();
        let mut index = self.load_index()?;
        let id = resolve(&index, key)?;
        let record = index
            .workspaces
            .iter()
            .find(|w| w.id == id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("workspace `{key}`")))?;

        let active = probe.active_work(&record);
        if !active.is_empty() && policy == ArchivePolicy::Admit {
            return Err(Error::WorkspaceActiveWork {
                id: record.id.clone(),
                details: summarize(&active),
            });
        }

        // Archive write FIRST — the gate precedes the work it would wake.
        self.write_marker(&PendingMutation::Archive {
            id: id.clone(),
            archived: true,
        })?;
        if let Some(slot) = index.workspaces.iter_mut().find(|w| w.id == id) {
            slot.archived = true;
        }
        self.save_index(&index)?;
        self.clear_marker(&id)?;

        // Stop only after the archive write is durable.
        let stopped = if policy == ArchivePolicy::StopThenArchive && !active.is_empty() {
            probe.stop(&record, &active)
        } else {
            Vec::new()
        };

        Ok(ArchiveOutcome {
            workspace: id,
            active_work: active,
            stopped,
        })
    }

    /// Refresh the header-only session index for a workspace. Reads each
    /// session's `.meta.json` only; transcript bodies are never loaded. Returns
    /// the number of sessions indexed.
    pub fn index_sessions(&self, key: &str) -> Result<usize> {
        let _guard = self.lock();
        let mut index = self.load_index()?;
        let id = resolve(&index, key)?;
        let root = PathBuf::from(&id);
        let headers = collect_session_headers(&root)?;
        let count = headers.len();
        if let Some(slot) = index.workspaces.iter_mut().find(|w| w.id == id) {
            slot.sessions = headers;
        }
        self.save_index(&index)?;
        Ok(count)
    }

    /// Finish any interrupted mutation left by a crash and clear its marker.
    ///
    /// The write-ahead order is kept on the way out too: every marker is read,
    /// the mutations are applied, and the index is rewritten **before** the
    /// first marker is unlinked. A crash mid-recovery therefore replays the
    /// markers instead of losing the mutation they describe.
    ///
    /// A marker that cannot be parsed is corruption, not something to guess
    /// around — it fails loud with the whole journal, well-formed markers
    /// included, left in place for inspection.
    pub fn recover(&self) -> Result<RecoveryReport> {
        let _guard = self.lock();
        let mut report = RecoveryReport::default();
        let mut index = self.load_index()?;
        let mut changed = false;

        let mut journal = Vec::new();
        for entry in self.read_markers()? {
            let bytes = std::fs::read(&entry).map_err(Error::Io)?;
            let mutation: PendingMutation =
                serde_json::from_slice(&bytes).map_err(|e| Error::WorkspaceCorrupt {
                    message: format!("cannot parse pending marker {}: {e}", entry.display()),
                })?;
            journal.push((entry, mutation));
        }

        for (_, mutation) in &journal {
            match mutation {
                PendingMutation::Create { record } => {
                    if index.workspaces.iter().any(|w| w.id == record.id) {
                        report.already_applied += 1;
                    } else {
                        index.workspaces.push(record.clone());
                        changed = true;
                        report.applied += 1;
                    }
                }
                PendingMutation::Delete { id } => {
                    let before = index.workspaces.len();
                    index.workspaces.retain(|w| &w.id != id);
                    if index.workspaces.len() == before {
                        report.already_applied += 1;
                    } else {
                        changed = true;
                        report.applied += 1;
                    }
                }
                PendingMutation::Archive { id, archived } => {
                    match index.workspaces.iter_mut().find(|w| &w.id == id) {
                        Some(w) if w.archived != *archived => {
                            w.archived = *archived;
                            changed = true;
                            report.applied += 1;
                        }
                        _ => report.already_applied += 1,
                    }
                }
            }
        }

        if changed {
            sort(&mut index.workspaces);
            self.save_index(&index)?;
        }

        // Only now that every effect is durable may a marker go away.
        for (entry, _) in &journal {
            std::fs::remove_file(entry).map_err(Error::Io)?;
            report.cleared += 1;
        }
        Ok(report)
    }

    /// Fail loud if the registry is internally inconsistent in a way no marker
    /// explains: a duplicate canonical path, an empty or non-absolute id, or an
    /// unparseable index (surfaced by [`Self::load_index`]).
    pub fn validate(&self) -> Result<()> {
        let index = self.load_index()?;
        let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
        for record in &index.workspaces {
            if record.id.is_empty() {
                return Err(Error::WorkspaceCorrupt {
                    message: "a record has an empty canonical id".into(),
                });
            }
            if !Path::new(&record.id).is_absolute() {
                return Err(Error::WorkspaceCorrupt {
                    message: format!("canonical id is not absolute: {}", record.id),
                });
            }
            if let Some(existing) = seen.insert(record.id.as_str(), record.name.as_str()) {
                return Err(Error::WorkspaceCorrupt {
                    message: format!(
                        "duplicate canonical path {} owned by `{existing}` and `{}`",
                        record.id, record.name
                    ),
                });
            }
        }
        Ok(())
    }

    fn read_markers(&self) -> Result<Vec<PathBuf>> {
        let dir = self.pending_dir();
        let mut markers = Vec::new();
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(markers),
            Err(e) => return Err(Error::Io(e)),
        };
        for entry in entries {
            let entry = entry.map_err(Error::Io)?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                markers.push(path);
            }
        }
        markers.sort();
        Ok(markers)
    }
}

/// Resolve a user-supplied key to a canonical registry id.
fn resolve(index: &RegistryIndex, key: &str) -> Result<String> {
    if index.workspaces.iter().any(|w| w.id == key) {
        return Ok(key.to_string());
    }
    if let Ok(canonical) = std::fs::canonicalize(key) {
        let canonical = canonical.to_string_lossy();
        if let Some(w) = index.workspaces.iter().find(|w| w.id == canonical) {
            return Ok(w.id.clone());
        }
    }
    if let Some(w) = index.workspaces.iter().find(|w| w.name == key) {
        return Ok(w.id.clone());
    }
    Err(Error::NotFound(format!("workspace `{key}`")))
}

fn realpath(path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => {
            Error::NotFound(format!("workspace path {}", path.display()))
        }
        _ => Error::Io(e),
    })
}

fn default_name(canonical: &Path) -> String {
    canonical
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| canonical.to_string_lossy().into_owned())
}

/// Keep the index deterministic: name-first then canonical id.
fn sort(records: &mut [WorkspaceRecord]) {
    records.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
}

fn summarize(work: &[ActiveWork]) -> String {
    work.iter()
        .map(|w| format!("{} {} ({})", w.kind.as_str(), w.id, w.detail))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Read only the `.meta.json` of each session under `root`'s session directory.
/// Transcript bodies are deliberately not touched.
fn collect_session_headers(root: &Path) -> Result<Vec<SessionHeader>> {
    let dirs = SessionReader::list_sessions(root).map_err(Error::Io)?;
    let mut headers = Vec::new();
    for dir in dirs {
        if let Ok(meta) = SessionReader::load_meta(&dir) {
            headers.push(SessionHeader::from(&meta));
        }
    }
    headers.sort_by(|a, b| a.updated_at.cmp(&b.updated_at));
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::PinnedRecursiveHome;
    use crate::workspace::activity::{ActiveWorkKind, NoActivityProbe};

    /// A probe that reports fixed active work, claims to have stopped all of it
    /// and records the archive flag it observed at stop time.
    struct FakeProbe {
        work: Vec<ActiveWork>,
        registry: WorkspaceRegistry,
        archived_at_stop: Mutex<Option<bool>>,
    }

    impl ActivityProbe for FakeProbe {
        fn active_work(&self, _record: &WorkspaceRecord) -> Vec<ActiveWork> {
            self.work.clone()
        }
        fn stop(&self, record: &WorkspaceRecord, work: &[ActiveWork]) -> Vec<ActiveWork> {
            let archived = self.registry.is_archived(&record.id).unwrap_or(false);
            *self.archived_at_stop.lock().unwrap() = Some(archived);
            work.to_vec()
        }
    }

    fn job(id: &str) -> ActiveWork {
        ActiveWork {
            kind: ActiveWorkKind::Job,
            id: id.to_string(),
            detail: "running".into(),
        }
    }

    /// Fresh per-test registry rooted in an isolated temp tree.
    fn registry() -> (tempfile::TempDir, PinnedRecursiveHome, WorkspaceRegistry) {
        let home = tempfile::tempdir().unwrap();
        let pin = PinnedRecursiveHome::new(home.path());
        let reg = WorkspaceRegistry::open().unwrap();
        (home, pin, reg)
    }

    #[test]
    fn create_registers_and_lists() {
        let (_home, _pin, reg) = registry();
        let ws = tempfile::tempdir().unwrap();
        let rec = reg.create(ws.path(), None).unwrap();
        assert_eq!(rec.name, ws.path().file_name().unwrap().to_string_lossy());
        assert!(Path::new(&rec.id).is_absolute());
        let all = reg.list().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, rec.id);
        assert!(!all[0].archived);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_registered_directory_is_rejected() {
        // The whole point of realpath canon: a different spelling of the same
        // directory cannot become a second entry.
        let (_home, _pin, reg) = registry();
        let ws = tempfile::tempdir().unwrap();
        let first = reg.create(ws.path(), Some("real".into())).unwrap();

        let link_dir = tempfile::tempdir().unwrap();
        let link = link_dir.path().join("alias");
        std::os::unix::fs::symlink(ws.path(), &link).unwrap();

        let err = reg.create(&link, Some("alias".into())).unwrap_err();
        match err {
            Error::WorkspaceConflict { canonical, .. } => assert_eq!(canonical, first.id),
            other => panic!("expected conflict, got {other:?}"),
        }
        assert_eq!(reg.list().unwrap().len(), 1, "no second entry may appear");
    }

    #[test]
    fn exact_directory_reregistration_is_rejected() {
        let (_home, _pin, reg) = registry();
        let ws = tempfile::tempdir().unwrap();
        reg.create(ws.path(), None).unwrap();
        assert!(matches!(
            reg.create(ws.path(), None).unwrap_err(),
            Error::WorkspaceConflict { .. }
        ));
    }

    #[test]
    fn remove_is_non_destructive() {
        let (_home, _pin, reg) = registry();
        let ws = tempfile::tempdir().unwrap();
        let file = ws.path().join("keep.txt");
        std::fs::write(&file, b"history").unwrap();
        let rec = reg.create(ws.path(), None).unwrap();

        reg.remove(&rec.id).unwrap();
        assert!(reg.list().unwrap().is_empty());
        // Directory, file, and everything under it survive.
        assert!(ws.path().exists(), "workspace dir must survive removal");
        assert!(file.exists(), "workspace files must survive removal");
        assert_eq!(std::fs::read(&file).unwrap(), b"history");
    }

    #[test]
    fn archive_blocks_on_active_work_under_admit() {
        let (_home, _pin, reg) = registry();
        let ws = tempfile::tempdir().unwrap();
        let rec = reg.create(ws.path(), None).unwrap();

        let probe = FakeProbe {
            work: vec![job("job-1")],
            registry: WorkspaceRegistry::at(reg.root()).unwrap(),
            archived_at_stop: Mutex::new(None),
        };
        let err = reg
            .archive(&rec.id, &probe, ArchivePolicy::Admit)
            .unwrap_err();
        match err {
            Error::WorkspaceActiveWork { id, details } => {
                assert_eq!(id, rec.id);
                assert!(details.contains("job-1"), "details: {details}");
            }
            other => panic!("expected active-work refusal, got {other:?}"),
        }
        assert!(!reg.is_archived(&rec.id).unwrap(), "must not archive");
    }

    #[test]
    fn stop_then_archive_persists_archive_before_stopping() {
        let (_home, _pin, reg) = registry();
        let ws = tempfile::tempdir().unwrap();
        let rec = reg.create(ws.path(), None).unwrap();

        let probe = FakeProbe {
            work: vec![job("job-1")],
            registry: WorkspaceRegistry::at(reg.root()).unwrap(),
            archived_at_stop: Mutex::new(None),
        };
        let outcome = reg
            .archive(&rec.id, &probe, ArchivePolicy::StopThenArchive)
            .unwrap();
        assert_eq!(outcome.stopped, outcome.active_work);
        assert_eq!(outcome.active_work.len(), 1);
        assert!(reg.is_archived(&rec.id).unwrap());
        // The archive write was durable at the moment stop ran.
        assert_eq!(*probe.archived_at_stop.lock().unwrap(), Some(true));
    }

    #[test]
    fn archive_with_no_work_succeeds_under_admit() {
        let (_home, _pin, reg) = registry();
        let ws = tempfile::tempdir().unwrap();
        let rec = reg.create(ws.path(), None).unwrap();
        let probe = NoActivityProbe;
        let outcome = reg.archive(&rec.id, &probe, ArchivePolicy::Admit).unwrap();
        assert!(outcome.active_work.is_empty());
        assert!(outcome.stopped.is_empty());
        assert!(reg.is_archived(&rec.id).unwrap());
        reg.set_archived(&rec.id, false).unwrap();
        assert!(!reg.is_archived(&rec.id).unwrap());
    }

    #[test]
    fn interrupted_create_self_heals_on_reopen() {
        let home = tempfile::tempdir().unwrap();
        let _pin = PinnedRecursiveHome::new(home.path());
        let ws = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(ws.path()).unwrap();

        let reg = WorkspaceRegistry::open().unwrap();
        // Simulate a crash between marker-write and index-write: the marker is
        // on disk, the index does not mention the workspace yet.
        let record = WorkspaceRecord {
            id: canonical.to_string_lossy().into_owned(),
            display: ws.path().to_string_lossy().into_owned(),
            name: "crashed".into(),
            created_at: crate::session::chrono_lite_now(),
            archived: false,
            sessions: Vec::new(),
        };
        reg.write_marker(&PendingMutation::Create {
            record: record.clone(),
        })
        .unwrap();
        assert!(reg.list().unwrap().is_empty(), "index not yet written");

        // Next start completes the create.
        let reopened = WorkspaceRegistry::open().unwrap();
        let all = reopened.list().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, record.id);
        assert!(
            reopened.read_markers().unwrap().is_empty(),
            "marker cleared"
        );
    }

    #[test]
    fn interrupted_delete_self_heals_on_reopen() {
        let home = tempfile::tempdir().unwrap();
        let _pin = PinnedRecursiveHome::new(home.path());
        let ws = tempfile::tempdir().unwrap();

        let reg = WorkspaceRegistry::open().unwrap();
        let rec = reg.create(ws.path(), None).unwrap();
        // Marker says "delete me" but the index still lists it.
        reg.write_marker(&PendingMutation::Delete { id: rec.id.clone() })
            .unwrap();

        let reopened = WorkspaceRegistry::open().unwrap();
        assert!(reopened.list().unwrap().is_empty());
    }

    #[test]
    fn duplicate_canonical_path_without_marker_fails_loud() {
        let home = tempfile::tempdir().unwrap();
        let _pin = PinnedRecursiveHome::new(home.path());
        let ws = tempfile::tempdir().unwrap();
        let reg = WorkspaceRegistry::open().unwrap();
        let rec = reg.create(ws.path(), None).unwrap();

        // Hand-craft an index with two records sharing one canonical path and
        // NO pending marker. This is exactly the "inconsistent, unexplained"
        // state that must not be silently repaired.
        let index = RegistryIndex {
            version: REGISTRY_SCHEMA_VERSION,
            workspaces: vec![rec.clone(), rec.clone()],
        };
        reg.save_index(&index).unwrap();

        let err = reg.validate().unwrap_err();
        assert!(matches!(err, Error::WorkspaceCorrupt { .. }), "{err:?}");
        // Opening the registry must also refuse the corrupt state.
        assert!(WorkspaceRegistry::open().is_err());
    }

    #[test]
    fn corrupt_marker_fails_loud() {
        let home = tempfile::tempdir().unwrap();
        let _pin = PinnedRecursiveHome::new(home.path());
        let reg = WorkspaceRegistry::open().unwrap();
        std::fs::write(reg.marker_path("whatever"), b"{ not json").unwrap();
        let err = reg.recover().unwrap_err();
        assert!(matches!(err, Error::WorkspaceCorrupt { .. }), "{err:?}");
        // The bad marker is left in place for inspection.
        assert!(reg.marker_path("whatever").exists());
    }

    #[test]
    fn corrupt_marker_leaves_the_well_formed_markers_pending() {
        // Recovery must not unlink anything it has not made durable: a good
        // marker is applied *and the index rewritten* before any marker goes
        // away, so a parse error aborts with the whole journal intact.
        let home = tempfile::tempdir().unwrap();
        let _pin = PinnedRecursiveHome::new(home.path());
        let ws = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(ws.path()).unwrap();
        let reg = WorkspaceRegistry::open().unwrap();

        let record = WorkspaceRecord {
            id: canonical.to_string_lossy().into_owned(),
            display: ws.path().to_string_lossy().into_owned(),
            name: "good".into(),
            created_at: crate::session::chrono_lite_now(),
            archived: false,
            sessions: Vec::new(),
        };
        reg.write_marker(&PendingMutation::Create {
            record: record.clone(),
        })
        .unwrap();
        // Sorts after any 16-hex-char marker name, so a recovery that unlinked
        // as it went would clear the good marker before reaching this one.
        std::fs::write(reg.pending_dir().join("zzzz-corrupt.json"), b"{ not json").unwrap();
        assert_eq!(reg.read_markers().unwrap().len(), 2);

        let err = reg.recover().unwrap_err();
        assert!(matches!(err, Error::WorkspaceCorrupt { .. }), "{err:?}");

        assert!(
            reg.list().unwrap().is_empty(),
            "no mutation reached the index"
        );
        assert_eq!(
            reg.read_markers().unwrap().len(),
            2,
            "every marker must survive a failed recovery"
        );
    }

    #[test]
    fn stop_then_archive_reports_only_what_the_probe_stopped() {
        // The probe reaches scheduled wakeups but not a live turn. The outcome
        // must say so, or the caller reports a stop that never happened.
        struct HalfProbe;
        impl ActivityProbe for HalfProbe {
            fn active_work(&self, _record: &WorkspaceRecord) -> Vec<ActiveWork> {
                vec![
                    ActiveWork {
                        kind: ActiveWorkKind::Turn,
                        id: "sess-1".into(),
                        detail: "working on it".into(),
                    },
                    ActiveWork {
                        kind: ActiveWorkKind::Schedule,
                        id: "t-1".into(),
                        detail: "wake up".into(),
                    },
                ]
            }
            fn stop(&self, _record: &WorkspaceRecord, work: &[ActiveWork]) -> Vec<ActiveWork> {
                work.iter()
                    .filter(|w| w.kind == ActiveWorkKind::Schedule)
                    .cloned()
                    .collect()
            }
        }

        let (_home, _pin, reg) = registry();
        let ws = tempfile::tempdir().unwrap();
        let rec = reg.create(ws.path(), None).unwrap();
        let outcome = reg
            .archive(&rec.id, &HalfProbe, ArchivePolicy::StopThenArchive)
            .unwrap();
        assert_eq!(outcome.active_work.len(), 2);
        assert_eq!(outcome.stopped.len(), 1);
        assert_eq!(outcome.stopped[0].id, "t-1");
    }

    #[test]
    fn registry_written_by_a_newer_schema_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let _pin = PinnedRecursiveHome::new(home.path());
        let reg = WorkspaceRegistry::open().unwrap();
        reg.save_index(&RegistryIndex {
            version: REGISTRY_SCHEMA_VERSION + 1,
            workspaces: Vec::new(),
        })
        .unwrap();

        let err = reg.list().unwrap_err();
        assert!(matches!(err, Error::WorkspaceCorrupt { .. }), "{err:?}");
        assert!(WorkspaceRegistry::open().is_err());
    }

    #[test]
    fn index_sessions_reads_only_headers() {
        let home = tempfile::tempdir().unwrap();
        let _pin = PinnedRecursiveHome::new(home.path());
        let ws = tempfile::tempdir().unwrap();
        let reg = WorkspaceRegistry::open().unwrap();
        let rec = reg.create(ws.path(), None).unwrap();

        // Write only a `.meta.json` (no transcript body at all). If the
        // indexer reached for the transcript it would fail or lose the row.
        let session_dir = crate::paths::user_sessions_dir(ws.path())
            .unwrap()
            .join("slug-1")
            .join("sess-1");
        std::fs::create_dir_all(&session_dir).unwrap();
        let meta = SessionMeta {
            schema_version: 1,
            session_id: "sess-1".into(),
            goal: "do the thing".into(),
            model: "m".into(),
            provider: "p".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:01Z".into(),
            message_count: 3,
            status: SessionStatus::Active,
            tool_registry_hash: None,
            first_prompt: Some("hi".into()),
            last_prompt: Some("hi".into()),
            cost: None,
            preset: None,
            name: Some("focused".into()),
            derived_from: None,
            finish_reason: None,
            error: None,
        };
        std::fs::write(
            session_dir.join(".meta.json"),
            serde_json::to_vec(&meta).unwrap(),
        )
        .unwrap();

        let count = reg.index_sessions(&rec.id).unwrap();
        assert_eq!(count, 1);
        let stored = reg.get(&rec.id).unwrap();
        assert_eq!(stored.sessions.len(), 1);
        assert_eq!(stored.sessions[0].session_id, "sess-1");
        assert_eq!(stored.sessions[0].status, SessionStatus::Active);
    }
}
