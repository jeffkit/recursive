//! Team roster persistence — `~/.claude/teams/{team_name}.json`.
//!
//! # Design
//!
//! A "team" is a flat roster of named agents (teammates) that the
//! coordinator can dispatch work to.  The roster is the *persisted* layer:
//! one JSON file per team under the user's data dir.  The in-memory
//! runtime counterparts (mailboxes, handles) live in `tasks.rs` and
//! `tools/agent.rs`.
//!
//! File format (`TeamFile`):
//! ```json
//! {
//!   "name": "alpha",
//!   "created_at": "2026-06-07T10:00:00Z",
//!   "members": [
//!     { "name": "researcher", "agent_type": "general", "status": "active", "model": "claude-opus-4-7" }
//!   ]
//! }
//! ```
//!
//! # Flat roster invariant
//!
//! Teammates are *flat* — a teammate's `manifest` cannot itself declare
//! teammates.  This is enforced in the `agent` tool (see
//! `src/tools/agent.rs`), not here.  `TeamRegistry` only stores the
//! roster shape; it does not validate what each teammate can spawn.
//!
//! # Atomicity
//!
//! All writes use a sibling-temp-file + rename pattern (see
//! `atomic_write` below) so a crash mid-write cannot leave a corrupt
//! `teams/*.json` on disk.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::error::{Error, Result};

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Directory under which per-team JSON files live.
///
/// Honors the `RECURSIVE_TEAMS_DIR` env var for tests; otherwise the
/// spec-mandated `~/.claude/teams/`.
pub fn teams_dir() -> PathBuf {
    if let Some(custom) = std::env::var_os("RECURSIVE_TEAMS_DIR") {
        return PathBuf::from(custom);
    }
    if let Some(home) = dirs::home_dir() {
        return home.join(".claude").join("teams");
    }
    PathBuf::from(".claude").join("teams")
}

/// Full path to a given team's roster file.
pub fn team_file_path(team_name: &str) -> PathBuf {
    teams_dir().join(format!("{team_name}.json"))
}

/// Make sure the teams directory exists.  Returns the path.
pub fn ensure_teams_dir() -> std::io::Result<PathBuf> {
    let dir = teams_dir();
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

// ---------------------------------------------------------------------------
// TeammateStatus / TeamMember
// ---------------------------------------------------------------------------

/// Runtime status of a teammate within a team.
///
/// Mirrors the spec:
/// - `Active`  — the teammate is currently running (or ready to be dispatched).
/// - `Idle`    — the teammate has been spawned but is not running anything.
/// - `Stopped` — the teammate was explicitly stopped by the coordinator.
/// - `Error`   — the teammate is in an error state (e.g. crashed during dispatch).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeammateStatus {
    Active,
    Idle,
    Stopped,
    Error,
}

/// A single member of a team's roster.
///
/// Kept intentionally small — runtime concerns (mailboxes, task IDs,
/// in-flight handles) live in `tasks.rs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMember {
    /// Display name of the teammate (unique within the team).
    pub name: String,

    /// Logical agent type (e.g. `"general"`, `"researcher"`, `"coder"`).
    /// Free-form — the coordinator uses it to pick allowed_tools / prompts.
    #[serde(default)]
    pub agent_type: String,

    /// Current status of this teammate.
    #[serde(default = "default_status")]
    pub status: TeammateStatus,

    /// Model identifier (free-form, e.g. `"claude-opus-4-7"`).
    /// Empty string means "inherit from parent".
    #[serde(default)]
    pub model: String,
}

fn default_status() -> TeammateStatus {
    TeammateStatus::Idle
}

impl TeamMember {
    pub fn new(name: impl Into<String>, agent_type: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            agent_type: agent_type.into(),
            status: default_status(),
            model: String::new(),
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn with_status(mut self, status: TeammateStatus) -> Self {
        self.status = status;
        self
    }
}

// ---------------------------------------------------------------------------
// TeamFile
// ---------------------------------------------------------------------------

/// The on-disk representation of a team.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamFile {
    /// Team name (matches the filename stem, e.g. `"alpha"`).
    pub name: String,

    /// When the team was first created.
    pub created_at: DateTime<Utc>,

    /// All members in the team, indexed by name for stable serialization.
    pub members: BTreeMap<String, TeamMember>,
}

impl TeamFile {
    /// Build a new, empty team file (no members yet).
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            created_at: Utc::now(),
            members: BTreeMap::new(),
        }
    }

    /// Add (or replace) a member.
    pub fn add_member(&mut self, member: TeamMember) {
        self.members.insert(member.name.clone(), member);
    }

    /// Remove a member by name.  Returns `true` if the member existed.
    pub fn remove_member(&mut self, name: &str) -> bool {
        self.members.remove(name).is_some()
    }

    /// Get a member by name.
    pub fn get_member(&self, name: &str) -> Option<&TeamMember> {
        self.members.get(name)
    }

    /// Mutable access to a member.
    pub fn get_member_mut(&mut self, name: &str) -> Option<&mut TeamMember> {
        self.members.get_mut(name)
    }

    /// Number of members in the team.
    pub fn member_count(&self) -> usize {
        self.members.len()
    }

    /// List of all member names (sorted, since backed by BTreeMap).
    pub fn member_names(&self) -> Vec<&str> {
        self.members.keys().map(|s| s.as_str()).collect()
    }
}

// ---------------------------------------------------------------------------
// Atomic write helper
// ---------------------------------------------------------------------------

/// Write `contents` to `path` atomically via a sibling temp file + rename.
///
/// The temp file lives in the same directory as `path` so the rename
/// stays on the same filesystem (atomic on POSIX).  This pattern is
/// copied from `src/session.rs::atomic_write` and adapted for use here
/// so the team registry doesn't have a hard dependency on `session.rs`.
pub(crate) fn atomic_write(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent")
    })?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".tmp-team-{}-{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("team"),
        std::process::id(),
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// TeamRegistry
// ---------------------------------------------------------------------------

/// In-memory + on-disk team registry.
///
/// The registry is cheap to clone — it's backed by an `Arc<RwLock<…>>`.
/// It is the single source of truth for *which teams exist* and *who
/// is on each team*.  Callers do not touch the filesystem directly;
/// they go through the registry methods (`load`, `save`, `create`,
/// `delete`, `add_member`, `remove_member`).
#[derive(Clone, Default)]
pub struct TeamRegistry {
    inner: Arc<RwLock<TeamRegistryInner>>,
}

#[derive(Default)]
struct TeamRegistryInner {
    /// team_name -> TeamFile (already loaded or freshly created).
    teams: BTreeMap<String, TeamFile>,
}

impl TeamRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a freshly created or loaded team into the in-memory
    /// registry. This is what `team_create` and `team_load` use so the
    /// registry is queryable via `get`/`list` after a tool call.
    /// Returns the previous value if a team with the same name existed.
    pub async fn register_team(&self, team: TeamFile) -> Option<TeamFile> {
        self.inner.write().await.teams.insert(team.name.clone(), team)
    }

    /// Load a team from disk.  Errors if the team file does not exist
    /// or is malformed.
    pub async fn load(team_name: &str) -> Result<Self> {
        let path = team_file_path(team_name);
        let bytes = std::fs::read(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::NotFound(format!("team '{team_name}'"))
            } else {
                Error::Other(format!("read team file: {e}"))
            }
        })?;
        let team: TeamFile = serde_json::from_slice(&bytes)?;
        let mut inner = TeamRegistryInner::default();
        inner.teams.insert(team.name.clone(), team);
        Ok(Self {
            inner: Arc::new(RwLock::new(inner)),
        })
    }

    /// Create a new empty team in memory and persist it.
    pub async fn create(team_name: &str) -> Result<Self> {
        let team = TeamFile::new(team_name);
        Self::save_team(&team)?;
        let mut inner = TeamRegistryInner::default();
        inner.teams.insert(team.name.clone(), team);
        Ok(Self {
            inner: Arc::new(RwLock::new(inner)),
        })
    }

    /// Persist the in-memory team to disk (atomic).
    pub async fn save(&self) -> Result<()> {
        let inner = self.inner.read().await;
        for team in inner.teams.values() {
            Self::save_team(team)?;
        }
        Ok(())
    }

    pub(crate) fn save_team(team: &TeamFile) -> Result<()> {
        ensure_teams_dir()?;
        let path = team_file_path(&team.name);
        let json = serde_json::to_string_pretty(team)?;
        atomic_write(&path, &json)?;
        Ok(())
    }

    /// Get a clone of the in-memory TeamFile.
    pub async fn get(&self, team_name: &str) -> Option<TeamFile> {
        self.inner.read().await.teams.get(team_name).cloned()
    }

    /// List all teams currently in memory.
    pub async fn list_teams(&self) -> Vec<String> {
        self.inner.read().await.teams.keys().cloned().collect()
    }

    /// List all teams that exist on disk.
    ///
    /// Reads the teams dir; useful for discovery (e.g. a `team_list`
    /// tool that wants to show all teams across the system).
    pub fn list_on_disk() -> std::io::Result<Vec<String>> {
        let dir = teams_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                // Skip temp files left behind by interrupted writes.
                if stem.starts_with(".tmp-") {
                    continue;
                }
                out.push(stem.to_string());
            }
        }
        out.sort();
        Ok(out)
    }

    /// Delete a team from memory and disk.
    pub async fn delete(team_name: &str) -> Result<bool> {
        let path = team_file_path(team_name);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(Error::Other(format!("delete team file: {e}"))),
        }
    }

    /// Add a member to a team (in-memory + persisted).
    pub async fn add_member(&self, team_name: &str, member: TeamMember) -> Result<()> {
        let mut inner = self.inner.write().await;
        let team = inner
            .teams
            .get_mut(team_name)
            .ok_or_else(|| Error::NotFound(format!("team '{team_name}'")))?;
        team.add_member(member);
        Self::save_team(team)?;
        Ok(())
    }

    /// Remove a member from a team (in-memory + persisted).
    /// Returns `true` if the member existed.
    pub async fn remove_member(&self, team_name: &str, member_name: &str) -> Result<bool> {
        let mut inner = self.inner.write().await;
        let team = inner
            .teams
            .get_mut(team_name)
            .ok_or_else(|| Error::NotFound(format!("team '{team_name}'")))?;
        let removed = team.remove_member(member_name);
        if removed {
            Self::save_team(team)?;
        }
        Ok(removed)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Serialize tests that touch the global teams dir.
    static TEAMS_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard: sets RECURSIVE_TEAMS_DIR for the duration of the guard,
    /// and restores the prior value on drop.  Pairs with `tempfile::tempdir`
    /// to give each test a fresh, isolated teams directory.
    struct TeamsDirGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        _tmp: tempfile::TempDir,
        prev: Option<std::ffi::OsString>,
    }

    fn with_temp_teams_dir() -> TeamsDirGuard {
        let lock = TEAMS_DIR_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var_os("RECURSIVE_TEAMS_DIR");
        std::env::set_var("RECURSIVE_TEAMS_DIR", tmp.path());
        TeamsDirGuard {
            _lock: lock,
            _tmp: tmp,
            prev,
        }
    }

    impl Drop for TeamsDirGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var("RECURSIVE_TEAMS_DIR", v),
                None => std::env::remove_var("RECURSIVE_TEAMS_DIR"),
            }
        }
    }

    #[test]
    fn team_file_new_is_empty() {
        let tf = TeamFile::new("alpha");
        assert_eq!(tf.name, "alpha");
        assert_eq!(tf.member_count(), 0);
        assert!(tf.member_names().is_empty());
        assert!(tf.created_at <= Utc::now());
    }

    #[test]
    fn add_and_remove_member() {
        let mut tf = TeamFile::new("alpha");
        let m = TeamMember::new("researcher", "general");
        tf.add_member(m.clone());
        assert_eq!(tf.member_count(), 1);
        assert_eq!(tf.get_member("researcher"), Some(&m));
        assert!(tf.remove_member("researcher"));
        assert_eq!(tf.member_count(), 0);
        assert!(!tf.remove_member("researcher"), "double-remove returns false");
    }

    #[test]
    fn team_member_with_model_and_status() {
        let m = TeamMember::new("coder", "general")
            .with_model("claude-opus-4-7")
            .with_status(TeammateStatus::Active);
        assert_eq!(m.model, "claude-opus-4-7");
        assert_eq!(m.status, TeammateStatus::Active);
    }

    #[test]
    fn team_file_round_trip_json() {
        let mut tf = TeamFile::new("alpha");
        tf.add_member(TeamMember::new("researcher", "general"));
        tf.add_member(
            TeamMember::new("coder", "general")
                .with_model("claude-opus-4-7")
                .with_status(TeammateStatus::Active),
        );
        let json = serde_json::to_string(&tf).unwrap();
        let back: TeamFile = serde_json::from_str(&json).unwrap();
        assert_eq!(tf, back);
    }

    #[tokio::test]
    async fn registry_create_save_load() {
        let _g = with_temp_teams_dir();
        let reg = TeamRegistry::create("alpha").await.unwrap();
        assert_eq!(reg.list_teams().await, vec!["alpha".to_string()]);
        assert!(team_file_path("alpha").exists());

        // Re-load: should produce an equal TeamFile.
        let reg2 = TeamRegistry::load("alpha").await.unwrap();
        let tf = reg2.get("alpha").await.unwrap();
        assert_eq!(tf.name, "alpha");
        assert_eq!(tf.member_count(), 0);
    }

    #[tokio::test]
    async fn registry_add_remove_member_persists() {
        let _g = with_temp_teams_dir();
        let reg = TeamRegistry::create("beta").await.unwrap();
        reg.add_member("beta", TeamMember::new("r", "general"))
            .await
            .unwrap();
        reg.add_member(
            "beta",
            TeamMember::new("c", "general").with_model("claude-opus-4-7"),
        )
        .await
        .unwrap();

        // Reload from disk and confirm both members are present.
        let reg2 = TeamRegistry::load("beta").await.unwrap();
        let tf = reg2.get("beta").await.unwrap();
        assert_eq!(tf.member_count(), 2);
        assert!(tf.get_member("r").is_some());
        assert!(tf.get_member("c").is_some());
        assert_eq!(tf.get_member("c").unwrap().model, "claude-opus-4-7");

        // Remove one and confirm.
        assert!(reg.remove_member("beta", "r").await.unwrap());
        let reg3 = TeamRegistry::load("beta").await.unwrap();
        assert_eq!(reg3.get("beta").await.unwrap().member_count(), 1);
    }

    #[tokio::test]
    async fn registry_delete_removes_file() {
        let _g = with_temp_teams_dir();
        let _ = TeamRegistry::create("gamma").await.unwrap();
        assert!(team_file_path("gamma").exists());
        assert!(TeamRegistry::delete("gamma").await.unwrap());
        assert!(!team_file_path("gamma").exists());
        // Second delete is idempotent: returns false, no error.
        assert!(!TeamRegistry::delete("gamma").await.unwrap());
    }

    #[tokio::test]
    async fn registry_load_missing_team_errors() {
        let _g = with_temp_teams_dir();
        let res = TeamRegistry::load("nonexistent").await;
        assert!(matches!(res, Err(Error::NotFound(_))));
    }

    #[test]
    fn atomic_write_replaces_existing() {
        let g = with_temp_teams_dir();
        let dir = g._tmp.path();
        let p = dir.join("alpha.json");
        atomic_write(&p, "{\"name\":\"alpha\"}").unwrap();
        let s1 = std::fs::read_to_string(&p).unwrap();
        assert_eq!(s1, "{\"name\":\"alpha\"}");

        atomic_write(&p, "{\"name\":\"alpha\",\"v\":2}").unwrap();
        let s2 = std::fs::read_to_string(&p).unwrap();
        assert_eq!(s2, "{\"name\":\"alpha\",\"v\":2}");

        // No leftover .tmp- files.
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            assert!(
                !entry
                    .file_name()
                    .to_str()
                    .unwrap()
                    .starts_with(".tmp-team-"),
                "found leftover temp file: {:?}",
                entry.file_name()
            );
        }
    }

    #[test]
    fn teammate_status_default_is_idle() {
        let j = r#"{"name":"x","agent_type":"general","model":""}"#;
        let m: TeamMember = serde_json::from_str(j).unwrap();
        assert_eq!(m.status, TeammateStatus::Idle);
    }
}
