//! Deliverables: an explicit "what did this turn deliver / change" surface.
//!
//! Two things live here, and they are deliberately separate:
//!
//! 1. **Presented deliverables** — the agent *declares* paths it considers
//!    delivered (via the `Present` tool). Declaring a path never copies
//!    bytes into the transcript: the model sees the path, the consumer sees
//!    an event.
//! 2. **The per-turn change ledger** — what actually changed on disk during
//!    the turn (added / modified / deleted, with bounded diffs). It comes
//!    from a zero-pollution git [`git_index::ShadowIndex`] when the
//!    workspace is a repository, and from a bounded, content-addressed
//!    [`capture::walk_snapshot`] otherwise.
//!
//! Every cost in this subsystem is bounded and every bound is *visible*:
//! oversized files, over-budget change counts, over-budget comparisons and
//! incomplete walks all degrade to an explicit `coarse` marker (or the
//! ledger-level `truncated` flag) instead of failing or silently dropping
//! the change.
//!
//! The baseline is taken lazily — immediately *before* the first mutating
//! tool call of the turn (see `tools::dispatch`) — so read-only turns pay
//! nothing and a change can never be captured after it happened.

pub mod capture;
pub mod compare;
pub mod git_index;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use capture::{CaptureStore, Fingerprint, Snapshot};
use git_index::{GitSnapshot, ShadowIndex};

/// Default ceiling on how many files one snapshot tracks / one ledger renders.
pub const DEFAULT_MAX_FILES: usize = 500;
/// Default ceiling on the size of a single captured file.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Default wall-clock budget for one file's line comparison.
pub const DEFAULT_COMPARE_DEADLINE_MS: u64 = 100;
/// Default ceiling on how many paths one `Present` call may declare.
pub const DEFAULT_MAX_PRESENT_FILES: usize = 8;
/// Hard ceiling on `Present` declarations, however it is configured.
pub const MAX_PRESENT_FILES_CEILING: usize = 32;

/// Every bound the ledger honours.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budgets {
    /// Maximum captured files per snapshot, and maximum rendered diffs per
    /// ledger.
    pub max_files: usize,
    /// Files larger than this are fingerprinted by size, never read.
    pub max_file_bytes: u64,
    /// Wall-clock budget for one file's line comparison.
    pub compare_deadline_ms: u64,
    /// Unchanged lines rendered around each hunk.
    pub context_lines: usize,
}

impl Default for Budgets {
    fn default() -> Self {
        Self {
            max_files: DEFAULT_MAX_FILES,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            compare_deadline_ms: DEFAULT_COMPARE_DEADLINE_MS,
            context_lines: compare::DEFAULT_CONTEXT_LINES,
        }
    }
}

impl Budgets {
    /// Read overrides from `RECURSIVE_DELIVERABLES_MAX_FILES`,
    /// `RECURSIVE_DELIVERABLES_MAX_FILE_BYTES` and
    /// `RECURSIVE_DELIVERABLES_COMPARE_MS`. Unparsable values are ignored
    /// (the default stands) rather than failing a run.
    pub fn from_env() -> Self {
        let mut budgets = Self::default();
        if let Some(v) = env_usize("RECURSIVE_DELIVERABLES_MAX_FILES") {
            budgets.max_files = v;
        }
        if let Some(v) = env_usize("RECURSIVE_DELIVERABLES_MAX_FILE_BYTES") {
            budgets.max_file_bytes = v as u64;
        }
        if let Some(v) = env_usize("RECURSIVE_DELIVERABLES_COMPARE_MS") {
            budgets.compare_deadline_ms = v as u64;
        }
        budgets
    }

    fn compare_deadline(&self) -> Duration {
        Duration::from_millis(self.compare_deadline_ms)
    }
}

fn env_usize(key: &str) -> Option<usize> {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
}

/// Is the deliverables subsystem enabled? On by default; `RECURSIVE_DELIVERABLES`
/// set to `0`/`false`/`off`/`no` turns it off.
pub fn enabled_from_env() -> bool {
    match std::env::var("RECURSIVE_DELIVERABLES") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        Err(_) => true,
    }
}

/// A path the agent explicitly declared as delivered this turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresentedFile {
    /// Workspace-relative, `/`-separated (or the path as given when it is
    /// outside the workspace but inside an allowed root).
    pub path: String,
    /// Size on disk at declaration time. The body itself never travels.
    pub bytes: u64,
}

/// A `Present` declaration that was refused, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresentRejection {
    pub path: String,
    pub reason: String,
}

/// Outcome of one `Present` call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresentOutcome {
    pub accepted: Vec<PresentedFile>,
    pub rejected: Vec<PresentRejection>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeStatus {
    Added,
    Modified,
    Deleted,
}

impl ChangeStatus {
    pub fn marker(self) -> char {
        match self {
            ChangeStatus::Added => '+',
            ChangeStatus::Modified => '~',
            ChangeStatus::Deleted => '-',
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ChangeStatus::Added => "added",
            ChangeStatus::Modified => "modified",
            ChangeStatus::Deleted => "deleted",
        }
    }
}

/// One changed file in the turn ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub status: ChangeStatus,
    pub bytes_before: Option<u64>,
    pub bytes_after: Option<u64>,
    /// Unified diff (empty when `coarse` replaced it, or when the path ran
    /// into the rendering budget).
    pub diff: String,
    /// The change is reported but its content comparison degraded.
    pub coarse: bool,
    pub coarse_reason: Option<String>,
}

/// The rendered, durable ledger for one turn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnChanges {
    pub turn: u32,
    pub added: Vec<FileChange>,
    pub modified: Vec<FileChange>,
    pub deleted: Vec<FileChange>,
    pub presented: Vec<PresentedFile>,
    /// Where the change set came from: `shadow_index` and/or `workspace_walk`.
    pub sources: Vec<String>,
    /// Baseline / current shadow-index tree ids (zero-pollution handles).
    pub baseline_tree: Option<String>,
    pub current_tree: Option<String>,
    /// The snapshot behind this ledger is not known to be complete — the
    /// `max_files` budget was hit, or a directory/entry could not be
    /// enumerated. The change set may be missing entries.
    pub truncated: bool,
}

impl TurnChanges {
    /// No changes, no presented files.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.modified.is_empty()
            && self.deleted.is_empty()
            && self.presented.is_empty()
    }

    pub fn changed_count(&self) -> usize {
        self.added.len() + self.modified.len() + self.deleted.len()
    }

    pub fn coarse_count(&self) -> usize {
        self.added
            .iter()
            .chain(&self.modified)
            .chain(&self.deleted)
            .filter(|c| c.coarse)
            .count()
    }

    pub fn all_changes(&self) -> impl Iterator<Item = &FileChange> {
        self.added.iter().chain(&self.modified).chain(&self.deleted)
    }

    /// One-line summary, e.g. `+2 ~1 -1 (1 coarse)`.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.added.is_empty() {
            parts.push(format!("+{}", self.added.len()));
        }
        if !self.modified.is_empty() {
            parts.push(format!("~{}", self.modified.len()));
        }
        if !self.deleted.is_empty() {
            parts.push(format!("-{}", self.deleted.len()));
        }
        if parts.is_empty() {
            return "no file changes".to_string();
        }
        let coarse = self.coarse_count();
        if coarse > 0 {
            parts.push(format!("({coarse} coarse)"));
        }
        parts.join(" ")
    }

    /// Render the ledger as text: header, per-file lines, then the diffs.
    /// Paths, sizes and statuses only — never a bare file body.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "turn {} change ledger — {} [{}]\n",
            self.turn,
            self.summary(),
            self.sources.join("+")
        ));
        if self.truncated {
            out.push_str(
                "  ! snapshot incomplete (budget or unreadable entries); the change set may be incomplete\n",
            );
        }
        for change in self.all_changes() {
            out.push_str(&format!(
                "  {} {} — {} ({}){}\n",
                change.status.marker(),
                change.path,
                change.status.as_str(),
                size_delta(change),
                match &change.coarse_reason {
                    Some(reason) => format!(" — coarse: {reason}"),
                    None => String::new(),
                }
            ));
        }
        if !self.presented.is_empty() {
            let listed: Vec<String> = self
                .presented
                .iter()
                .map(|f| format!("{} ({})", f.path, human_bytes(f.bytes)))
                .collect();
            out.push_str(&format!("  delivered: {}\n", listed.join(", ")));
        }
        for change in self.all_changes() {
            if change.diff.is_empty() {
                continue;
            }
            out.push('\n');
            out.push_str(&change.diff);
        }
        out
    }
}

fn size_delta(change: &FileChange) -> String {
    match (change.bytes_before, change.bytes_after) {
        (Some(before), Some(after)) => format!("{} → {}", human_bytes(before), human_bytes(after)),
        (None, Some(after)) => human_bytes(after),
        (Some(before), None) => human_bytes(before),
        (None, None) => "?".to_string(),
    }
}

/// Compact byte size: `512 B`, `1.5 KiB`, `2.0 MiB`.
pub fn human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KIB {
        format!("{bytes} B")
    } else if b < KIB * KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{:.1} MiB", b / (KIB * KIB))
    }
}

/// The workspace state at one instant, however it was obtained.
#[derive(Debug, Clone)]
enum State {
    Git(GitSnapshot),
    Walk(Snapshot),
}

impl State {
    fn source(&self) -> &'static str {
        match self {
            State::Git(_) => "shadow_index",
            State::Walk(_) => "workspace_walk",
        }
    }

    fn tree(&self) -> Option<String> {
        match self {
            State::Git(s) => s.tree.clone(),
            State::Walk(_) => None,
        }
    }

    fn truncated(&self) -> bool {
        match self {
            State::Git(_) => false,
            State::Walk(s) => s.truncated,
        }
    }
}

#[derive(Debug, Default)]
struct LedgerState {
    turn: u32,
    /// A turn was started — the ledger is tracking, so a baseline is worth
    /// taking. Without this, invoking a mutating tool outside a runtime turn
    /// would trigger a whole-workspace snapshot for nothing.
    armed: bool,
    baseline: Option<State>,
    presented: Vec<PresentedFile>,
    /// Last finalized ledger, and whether the turn it belongs to is over.
    last: Option<TurnChanges>,
    finalized: bool,
}

/// One change, as the snapshot source sees it, before rendering.
struct ChangeInput {
    status: ChangeStatus,
    /// Baseline body; `None` means "unavailable" (render degrades).
    old: Option<Vec<u8>>,
    /// Current body; `None` means "unavailable" (render degrades).
    new: Option<Vec<u8>>,
    /// Size on the baseline side when no body could be read.
    old_size: Option<u64>,
    /// Size on the current side when no body could be read.
    new_size: Option<u64>,
    /// A degradation the source already knows about (over-budget capture,
    /// unreadable file, …).
    coarse: Option<String>,
}

/// Session-scoped deliverables ledger.
///
/// Cheap to share (`Arc<Deliverables>`) between the runtime, the tool
/// registry and the `Present` tool so all three see the same turn state.
#[derive(Debug)]
pub struct Deliverables {
    workspace: PathBuf,
    root: PathBuf,
    budgets: Budgets,
    store: CaptureStore,
    index: Mutex<Option<ShadowIndex>>,
    probed: AtomicBool,
    state: Mutex<LedgerState>,
}

fn tool_error(message: impl Into<String>) -> Error {
    Error::Tool {
        name: "deliverables".into(),
        call_id: None,
        message: message.into(),
    }
}

impl Deliverables {
    /// Build a ledger for `workspace`, keeping private state (blob store,
    /// shadow index) under `root`. `root` must live *outside* the workspace,
    /// otherwise the shadow index would index its own scratch files.
    pub fn new(
        workspace: impl Into<PathBuf>,
        root: impl Into<PathBuf>,
        budgets: Budgets,
    ) -> Result<Self> {
        let workspace = workspace.into();
        let root = root.into();
        let store = CaptureStore::open(root.join("capture"))?;
        Ok(Self {
            workspace,
            root,
            budgets,
            store,
            index: Mutex::new(None),
            probed: AtomicBool::new(false),
            state: Mutex::new(LedgerState::default()),
        })
    }

    /// Build a ledger using the per-user, per-workspace data dir and
    /// environment-tuned budgets.
    pub fn for_workspace(workspace: &Path) -> Result<Self> {
        let root = crate::paths::user_workspace_dir(workspace)?.join("deliverables");
        Self::new(workspace, root, Budgets::from_env())
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn budgets(&self) -> &Budgets {
        &self.budgets
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Arm the ledger for `turn`: forget the previous baseline and, with it,
    /// the previous turn's changes. The baseline itself is captured lazily by
    /// [`Self::ensure_baseline`], so read-only turns cost nothing.
    pub fn begin_turn(&self, turn: u32) {
        if let Ok(mut state) = self.state.lock() {
            state.turn = turn;
            state.armed = true;
            state.baseline = None;
            state.presented.clear();
            state.last = None;
            state.finalized = false;
        }
    }

    pub fn current_turn(&self) -> u32 {
        self.state.lock().map(|s| s.turn).unwrap_or_default()
    }

    /// Capture the turn baseline if it has not been captured yet.
    ///
    /// Called *before* the first mutating tool call of the turn, and again
    /// (idempotently) at finalize time. A no-op until [`Self::begin_turn`]
    /// armed the ledger (invoking a tool outside a runtime turn has no turn
    /// to attribute changes to, and must not pay for a snapshot). Never
    /// fails the caller's turn: an unavailable git or an unreadable
    /// workspace simply selects the walk fallback.
    pub fn ensure_baseline(&self) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| tool_error("deliverables state lock poisoned"))?;
        if state.baseline.is_some() || !state.armed {
            return Ok(());
        }
        state.baseline = Some(self.snapshot()?);
        Ok(())
    }

    pub fn has_baseline(&self) -> bool {
        self.state
            .lock()
            .map(|s| s.baseline.is_some())
            .unwrap_or(false)
    }

    fn snapshot(&self) -> Result<State> {
        if let Some(index) = self.shadow_index()? {
            return Ok(State::Git(index.snapshot()?));
        }
        Ok(State::Walk(capture::walk_snapshot(
            &self.store,
            &self.workspace,
            self.budgets.max_files,
            self.budgets.max_file_bytes,
        )))
    }

    /// Open (once) the workspace shadow index; `None` when the workspace is
    /// not a git repository or git is unavailable.
    fn shadow_index(&self) -> Result<Option<ShadowIndex>> {
        if self.probed.load(Ordering::Relaxed) {
            return Ok(self
                .index
                .lock()
                .map_err(|_| tool_error("deliverables index lock poisoned"))?
                .clone());
        }
        let opened = ShadowIndex::open(&self.workspace, self.root.join("shadow-git"));
        let opened = match opened {
            Ok(index) => Some(index),
            Err(err) => {
                tracing::debug!(error = %err, "deliverables: shadow index unavailable, falling back to workspace walk");
                None
            }
        };
        {
            let mut slot = self
                .index
                .lock()
                .map_err(|_| tool_error("deliverables index lock poisoned"))?;
            *slot = opened;
        }
        self.probed.store(true, Ordering::Relaxed);
        Ok(self
            .index
            .lock()
            .map_err(|_| tool_error("deliverables index lock poisoned"))?
            .clone())
    }

    /// Record declared deliverables for the current turn, enforcing
    /// `max_files`. Returns what was accepted and what was refused (with the
    /// reason) — a partial declaration is never silent.
    pub fn present(&self, files: &[PresentedFile], max_files: usize) -> PresentOutcome {
        let cap = max_files.min(MAX_PRESENT_FILES_CEILING);
        let mut outcome = PresentOutcome::default();
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => {
                outcome.rejected.push(PresentRejection {
                    path: "<ledger>".into(),
                    reason: "deliverables state lock poisoned".into(),
                });
                return outcome;
            }
        };
        for file in files {
            if outcome.accepted.iter().any(|f| f.path == file.path)
                || state.presented.iter().any(|f| f.path == file.path)
            {
                outcome.rejected.push(PresentRejection {
                    path: file.path.clone(),
                    reason: "already presented this turn".into(),
                });
                continue;
            }
            if state.presented.len() + outcome.accepted.len() >= cap {
                outcome.rejected.push(PresentRejection {
                    path: file.path.clone(),
                    reason: format!("beyond the maxFiles={cap} budget"),
                });
                continue;
            }
            outcome.accepted.push(file.clone());
        }
        state.presented.extend(outcome.accepted.iter().cloned());
        outcome
    }

    pub fn presented(&self) -> Vec<PresentedFile> {
        self.state
            .lock()
            .map(|s| s.presented.clone())
            .unwrap_or_default()
    }

    /// Close the turn: snapshot again, diff against the baseline, render the
    /// ledger and remember it. A turn with no mutating tool call has no
    /// baseline and therefore no changes.
    pub fn finalize_turn(&self, turn: u32) -> Result<TurnChanges> {
        let changes = self.compute_changes(turn)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| tool_error("deliverables state lock poisoned"))?;
        state.last = Some(changes.clone());
        state.finalized = true;
        Ok(changes)
    }

    /// The most recently finalized ledger, if any.
    pub fn last_changes(&self) -> Option<TurnChanges> {
        self.state.lock().ok().and_then(|s| s.last.clone())
    }

    /// Build the current turn's ledger without recording it as final.
    fn compute_changes(&self, turn: u32) -> Result<TurnChanges> {
        let (baseline, presented) = {
            let state = self
                .state
                .lock()
                .map_err(|_| tool_error("deliverables state lock poisoned"))?;
            (state.baseline.clone(), state.presented.clone())
        };

        let mut changes = TurnChanges {
            turn,
            presented,
            ..Default::default()
        };
        if let Some(baseline) = baseline {
            let current = self.snapshot()?;
            changes.sources = vec![baseline.source().to_string()];
            changes.baseline_tree = baseline.tree();
            changes.current_tree = current.tree();
            changes.truncated = baseline.truncated() || current.truncated();
            let mut files = self.diff(&baseline, &current)?;
            files.sort_by(|a, b| a.path.cmp(&b.path));
            for change in files {
                match change.status {
                    ChangeStatus::Added => changes.added.push(change),
                    ChangeStatus::Modified => changes.modified.push(change),
                    ChangeStatus::Deleted => changes.deleted.push(change),
                }
            }
            self.apply_render_budget(&mut changes);
        }
        Ok(changes)
    }

    /// The current turn's ledger: the finalized one once the turn is over,
    /// otherwise a fresh live diff against the baseline (so a mid-turn read
    /// never returns a stale snapshot). `None` when nothing has happened
    /// this turn — no baseline means no non-read-only tool ran.
    pub fn changes(&self) -> Result<Option<TurnChanges>> {
        let turn = self.current_turn();
        if let Ok(state) = self.state.lock() {
            if state.finalized {
                if let Some(last) = &state.last {
                    if last.turn == turn {
                        return Ok(Some(last.clone()));
                    }
                }
            }
        }
        if !self.has_baseline() {
            return Ok(None);
        }
        Ok(Some(self.compute_changes(turn)?))
    }

    /// Render the current turn's ledger, if it has anything to say.
    pub fn render(&self) -> Result<Option<String>> {
        Ok(self
            .changes()?
            .filter(|c| !c.is_empty())
            .map(|c| c.render()))
    }

    fn diff(&self, baseline: &State, current: &State) -> Result<Vec<FileChange>> {
        match (baseline, current) {
            (State::Git(base), State::Git(cur)) => self.diff_git(base, cur),
            (State::Walk(base), State::Walk(cur)) => self.diff_walk(base, cur),
            // The mode can only change if git appears/disappears mid-turn.
            _ => Ok(Vec::new()),
        }
    }

    fn diff_git(&self, base: &GitSnapshot, cur: &GitSnapshot) -> Result<Vec<FileChange>> {
        let mut paths: BTreeSet<&String> = BTreeSet::new();
        paths.extend(base.paths.keys());
        paths.extend(cur.paths.keys());

        let mut changes = Vec::new();
        let Some(index) = self.shadow_index()? else {
            return Ok(changes);
        };
        for path in paths {
            let before = base.paths.get(path);
            let after = cur.paths.get(path);
            if before == after {
                continue;
            }
            let status = match (before, after) {
                (Some(_), Some(_)) => ChangeStatus::Modified,
                (None, Some(_)) => ChangeStatus::Added,
                (Some(_), None) => ChangeStatus::Deleted,
                (None, None) => continue,
            };
            let old_bytes = match before {
                Some(oid) => index.read_blob(oid).ok(),
                None => Some(Vec::new()),
            };
            let new_bytes = match after {
                Some(oid) => index.read_blob(oid).ok(),
                None => Some(Vec::new()),
            };
            changes.push(self.build_change(
                path,
                ChangeInput {
                    status,
                    old: old_bytes,
                    new: new_bytes,
                    old_size: None,
                    new_size: None,
                    coarse: None,
                },
            ));
        }
        Ok(changes)
    }

    fn diff_walk(&self, base: &Snapshot, cur: &Snapshot) -> Result<Vec<FileChange>> {
        let mut paths: BTreeSet<&String> = BTreeSet::new();
        paths.extend(base.files.keys());
        paths.extend(cur.files.keys());

        let mut changes = Vec::new();
        for path in paths {
            let before = base.files.get(path);
            let after = cur.files.get(path);
            match (before, after) {
                (Some(a), Some(b)) if a.same_content(b) => continue,
                (Some(_), Some(_)) => {}
                (None, Some(_)) => {}
                (Some(_), None) => {}
                (None, None) => continue,
            }
            let status = match (before, after) {
                (Some(_), Some(_)) => ChangeStatus::Modified,
                (None, Some(_)) => ChangeStatus::Added,
                (Some(_), None) => ChangeStatus::Deleted,
                (None, None) => continue,
            };
            let old_bytes = match before {
                Some(fp) => self.read_fingerprint(fp),
                None => Some(Vec::new()),
            };
            let new_bytes = match after {
                Some(fp) => self.read_fingerprint(fp),
                None => Some(Vec::new()),
            };
            let coarse = before
                .and_then(|fp| self.fingerprint_reason(fp))
                .or_else(|| after.and_then(|fp| self.fingerprint_reason(fp)));
            changes.push(self.build_change(
                path,
                ChangeInput {
                    status,
                    old: old_bytes,
                    new: new_bytes,
                    old_size: before.map(|fp| fp.bytes()),
                    new_size: after.map(|fp| fp.bytes()),
                    coarse,
                },
            ));
        }
        Ok(changes)
    }

    fn read_fingerprint(&self, fingerprint: &Fingerprint) -> Option<Vec<u8>> {
        match fingerprint {
            Fingerprint::Content { sha1, .. } => self.store.get(sha1).ok().flatten(),
            Fingerprint::Oversized { .. } | Fingerprint::Unreadable { .. } => None,
        }
    }

    /// The explicit reason a fingerprint cannot be compared by content.
    fn fingerprint_reason(&self, fingerprint: &Fingerprint) -> Option<String> {
        match fingerprint {
            Fingerprint::Content { .. } => None,
            Fingerprint::Oversized { .. } => Some(format!(
                "file exceeds the {} byte capture budget",
                self.budgets.max_file_bytes
            )),
            Fingerprint::Unreadable { .. } => Some("file could not be read".to_string()),
        }
    }

    /// Turn raw before/after bodies into a rendered [`FileChange`], applying
    /// the byte, type and time budgets.
    fn build_change(&self, path: &str, input: ChangeInput) -> FileChange {
        let status = input.status;
        let bytes_before = match status {
            ChangeStatus::Added => None,
            _ => input
                .old
                .as_ref()
                .map(|b| b.len() as u64)
                .or(input.old_size),
        };
        let bytes_after = match status {
            ChangeStatus::Deleted => None,
            _ => input
                .new
                .as_ref()
                .map(|b| b.len() as u64)
                .or(input.new_size),
        };
        if let Some(reason) = input.coarse {
            return self.coarse_change(path, status, bytes_before, bytes_after, reason);
        }
        let over_budget = |bytes: Option<u64>| {
            bytes
                .map(|b| b > self.budgets.max_file_bytes)
                .unwrap_or(false)
        };
        if over_budget(bytes_before) || over_budget(bytes_after) {
            return self.coarse_change(
                path,
                status,
                bytes_before,
                bytes_after,
                format!(
                    "file exceeds the {} byte capture budget",
                    self.budgets.max_file_bytes
                ),
            );
        }
        let (Some(old), Some(new)) = (input.old, input.new) else {
            return self.coarse_change(
                path,
                status,
                bytes_before,
                bytes_after,
                "content unavailable (unreadable file or absent blob)".to_string(),
            );
        };
        let (Ok(old_text), Ok(new_text)) = (
            String::from_utf8(old).map_err(|_| ()),
            String::from_utf8(new).map_err(|_| ()),
        ) else {
            return self.coarse_change(
                path,
                status,
                bytes_before,
                bytes_after,
                "binary or non-UTF-8 content".to_string(),
            );
        };

        let outcome = compare::unified_diff(
            path,
            &old_text,
            &new_text,
            self.budgets.context_lines,
            self.budgets.compare_deadline(),
        );
        FileChange {
            path: path.to_string(),
            status,
            bytes_before,
            bytes_after,
            diff: outcome.text,
            coarse: outcome.coarse.is_some(),
            coarse_reason: outcome.coarse,
        }
    }

    fn coarse_change(
        &self,
        path: &str,
        status: ChangeStatus,
        bytes_before: Option<u64>,
        bytes_after: Option<u64>,
        reason: String,
    ) -> FileChange {
        FileChange {
            path: path.to_string(),
            status,
            bytes_before,
            bytes_after,
            diff: String::new(),
            coarse: true,
            coarse_reason: Some(reason),
        }
    }

    /// Apply the per-ledger diff budget: at most `max_files` changes are
    /// rendered with content; the rest are still listed, marked coarse.
    pub fn apply_render_budget(&self, changes: &mut TurnChanges) {
        let budget = self.budgets.max_files;
        let mut seen = 0usize;
        for change in changes
            .added
            .iter_mut()
            .chain(changes.modified.iter_mut())
            .chain(changes.deleted.iter_mut())
        {
            seen += 1;
            if seen > budget {
                change.diff.clear();
                change.coarse = true;
                change.coarse_reason = Some(format!("beyond the maxFiles={budget} render budget"));
            }
        }
        changes.truncated |= seen > budget;
    }

    /// Files whose path names a real, readable entry under `abs`.
    ///
    /// Directories (and every other non-regular entry) are not deliverables —
    /// declaring one would report a directory as a delivered "file" with the
    /// directory's size — so they describe nothing.
    pub fn describe(&self, abs: &Path) -> Option<PresentedFile> {
        let meta = std::fs::metadata(abs).ok()?;
        if !meta.is_file() {
            return None;
        }
        let len = meta.len();
        let rel = abs
            .strip_prefix(&self.workspace)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| abs.to_string_lossy().to_string());
        Some(PresentedFile {
            path: rel,
            bytes: len,
        })
    }

    /// Path of the content-addressed blob store (test/diagnostics handle).
    pub fn capture_store(&self) -> &CaptureStore {
        &self.store
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git required for these tests");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init", "-q"]);
        git(dir, &["config", "user.email", "t@example.com"]);
        git(dir, &["config", "user.name", "t"]);
    }

    fn ledger(ws: &Path, private: &Path) -> Deliverables {
        Deliverables::new(ws, private, Budgets::default()).unwrap()
    }

    /// A repo workspace plus a private (out-of-tree) ledger root.
    fn repo_fixture() -> (TempDir, PathBuf, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        init_repo(&ws);
        let private = tmp.path().join("private");
        (tmp, ws, private)
    }

    #[test]
    fn turn_ledger_reports_added_modified_deleted_with_diffs() {
        let (_tmp, ws, private) = repo_fixture();
        std::fs::write(ws.join("keep.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(ws.join("gone.txt"), "bye\n").unwrap();
        let ledger = ledger(&ws, &private);

        ledger.begin_turn(1);
        ledger.ensure_baseline().unwrap();
        std::fs::write(ws.join("keep.txt"), "one\nTWO\nthree\n").unwrap();
        std::fs::write(ws.join("new.txt"), "fresh\n").unwrap();
        std::fs::remove_file(ws.join("gone.txt")).unwrap();

        let changes = ledger.finalize_turn(1).unwrap();
        assert_eq!(changes.sources, vec!["shadow_index".to_string()]);
        assert_eq!(changes.added.len(), 1, "{changes:?}");
        assert_eq!(changes.modified.len(), 1, "{changes:?}");
        assert_eq!(changes.deleted.len(), 1, "{changes:?}");
        assert_eq!(changes.added[0].path, "new.txt");
        assert_eq!(changes.modified[0].path, "keep.txt");
        assert_eq!(changes.deleted[0].path, "gone.txt");
        assert!(changes.baseline_tree.is_some());

        let modified = &changes.modified[0];
        assert!(!modified.coarse);
        assert!(modified.diff.contains("+TWO\n"), "{}", modified.diff);
        assert!(modified.diff.contains("-two\n"), "{}", modified.diff);
        assert_eq!(modified.bytes_before, Some(14));
        assert_eq!(modified.bytes_after, Some(14));

        let rendered = changes.render();
        assert!(rendered.contains("~ keep.txt — modified"), "{rendered}");
        assert!(rendered.contains("+ new.txt — added"), "{rendered}");
        assert!(rendered.contains("- gone.txt — deleted"), "{rendered}");
    }

    #[test]
    fn unrelated_user_edits_are_not_absorbed_into_a_turn() {
        let (_tmp, ws, private) = repo_fixture();
        std::fs::write(ws.join("a.txt"), "committed\n").unwrap();
        git(&ws, &["add", "--all"]);
        git(&ws, &["commit", "-q", "-m", "init", "--no-gpg-sign"]);
        // The user leaves an uncommitted edit behind…
        std::fs::write(ws.join("a.txt"), "committed\nuser\n").unwrap();
        let ledger = ledger(&ws, &private);

        // …which lands in the baseline, so the agent's turn starts from it.
        ledger.begin_turn(1);
        ledger.ensure_baseline().unwrap();
        std::fs::write(ws.join("b.txt"), "agent\n").unwrap();
        let changes = ledger.finalize_turn(1).unwrap();

        assert_eq!(changes.added.len(), 1);
        assert_eq!(changes.added[0].path, "b.txt");
        assert!(
            changes.modified.is_empty(),
            "user edit must not be a turn change: {changes:?}"
        );
    }

    #[test]
    fn the_user_repository_is_never_polluted() {
        let (_tmp, ws, private) = repo_fixture();
        std::fs::write(ws.join("a.txt"), "one\n").unwrap();
        git(&ws, &["add", "--all"]);
        git(&ws, &["commit", "-q", "-m", "init", "--no-gpg-sign"]);
        let index_before = std::fs::read(ws.join(".git/index")).unwrap();
        let head_before = std::fs::read(ws.join(".git/HEAD")).unwrap();
        let status_before = run_git_status(&ws);
        assert_eq!(status_before, "", "fixture must start clean");

        let ledger = ledger(&ws, &private);
        ledger.begin_turn(7);
        ledger.ensure_baseline().unwrap();
        std::fs::write(ws.join("a.txt"), "one\ntwo\n").unwrap();
        ledger.finalize_turn(7).unwrap();

        assert_eq!(std::fs::read(ws.join(".git/index")).unwrap(), index_before);
        assert_eq!(std::fs::read(ws.join(".git/HEAD")).unwrap(), head_before);
        // The agent's own edit is a plain *unstaged* working-tree change:
        // nothing got staged, committed or stashed on the user's behalf.
        assert_eq!(run_git_status(&ws), " M a.txt\n");
        assert!(!ws.join(".git/index.lock").exists());
        assert!(private.join("shadow-git/index").exists());
    }

    fn run_git_status(dir: &Path) -> String {
        let out = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(dir)
            .output()
            .expect("git required");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn read_only_turns_cost_nothing_and_report_nothing() {
        let (_tmp, ws, private) = repo_fixture();
        std::fs::write(ws.join("a.txt"), "one\n").unwrap();
        let ledger = ledger(&ws, &private);
        ledger.begin_turn(3);
        assert!(!ledger.has_baseline());
        let changes = ledger.finalize_turn(3).unwrap();
        assert!(changes.is_empty());
        assert!(changes.sources.is_empty());
    }

    #[test]
    fn non_git_workspace_falls_back_to_the_content_addressed_walk() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("plain");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("a.txt"), "one\n").unwrap();
        let ledger = ledger(&ws, &tmp.path().join("private"));

        ledger.begin_turn(1);
        ledger.ensure_baseline().unwrap();
        std::fs::write(ws.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(ws.join("b.txt"), "new\n").unwrap();
        let changes = ledger.finalize_turn(1).unwrap();

        assert_eq!(changes.sources, vec!["workspace_walk".to_string()]);
        assert_eq!(changes.added.len(), 1);
        assert_eq!(changes.modified.len(), 1);
        assert!(
            changes.modified[0].diff.contains("+two\n"),
            "{:?}",
            changes.modified[0]
        );
        // Bodies are content-addressed: two distinct blobs, no third copy.
        assert!(ledger
            .capture_store()
            .contains(&capture::sha1_hex(b"one\n")));
    }

    #[test]
    fn identical_bodies_across_paths_share_one_blob() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("plain");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("a.txt"), "same\n").unwrap();
        std::fs::write(ws.join("b.txt"), "same\n").unwrap();
        let ledger = ledger(&ws, &tmp.path().join("private"));
        ledger.begin_turn(1);
        ledger.ensure_baseline().unwrap();
        let blobs = std::fs::read_dir(ledger.capture_store().root().join("blobs")).unwrap();
        assert_eq!(blobs.count(), 1, "content addressing must dedupe by bytes");
    }

    #[test]
    fn oversized_files_degrade_explicitly_instead_of_failing() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("plain");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("big.bin"), vec![b'x'; 2048]).unwrap();
        let budgets = Budgets {
            max_file_bytes: 16,
            ..Budgets::default()
        };
        let ledger = Deliverables::new(&ws, tmp.path().join("private"), budgets).unwrap();

        ledger.begin_turn(1);
        ledger.ensure_baseline().unwrap();
        std::fs::write(ws.join("big.bin"), vec![b'y'; 2049]).unwrap();
        let changes = ledger.finalize_turn(1).unwrap();

        assert_eq!(changes.modified.len(), 1);
        let change = &changes.modified[0];
        assert!(change.coarse);
        assert!(
            change
                .coarse_reason
                .clone()
                .unwrap_or_default()
                .contains("capture budget"),
            "{change:?}"
        );
        assert!(changes.summary().contains("coarse"));
    }

    #[test]
    fn render_budget_marks_the_tail_of_a_large_change_set_coarse() {
        let (_tmp, ws, private) = repo_fixture();
        let ledger = Deliverables::new(
            &ws,
            &private,
            Budgets {
                max_files: 2,
                ..Budgets::default()
            },
        )
        .unwrap();
        ledger.begin_turn(1);
        ledger.ensure_baseline().unwrap();
        for i in 0..5 {
            std::fs::write(ws.join(format!("f{i}.txt")), "x\n").unwrap();
        }
        let changes = ledger.finalize_turn(1).unwrap();
        assert_eq!(changes.added.len(), 5);
        assert!(changes.truncated);
        let coarse: Vec<&FileChange> = changes.added.iter().filter(|c| c.coarse).collect();
        assert_eq!(coarse.len(), 3, "{changes:?}");
        assert!(coarse[0]
            .coarse_reason
            .clone()
            .unwrap_or_default()
            .contains("render budget"));
    }

    #[test]
    fn present_enforces_the_max_files_budget_and_dedupes() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("plain");
        std::fs::create_dir_all(&ws).unwrap();
        let ledger = ledger(&ws, &tmp.path().join("private"));
        ledger.begin_turn(4);

        let files: Vec<PresentedFile> = (0..4)
            .map(|i| PresentedFile {
                path: format!("f{i}.rs"),
                bytes: 10,
            })
            .collect();
        let first = ledger.present(&files, 3);
        assert_eq!(first.accepted.len(), 3);
        assert_eq!(first.rejected.len(), 1);
        assert!(first.rejected[0].reason.contains("maxFiles=3"));

        // A second call dedupes against what the turn already declared.
        let second = ledger.present(&files[..1], 3);
        assert!(second.accepted.is_empty());
        assert!(second.rejected[0].reason.contains("already presented"));

        // …and the cap is still enforced across calls.
        let third = ledger.present(
            &[PresentedFile {
                path: "extra.rs".into(),
                bytes: 1,
            }],
            3,
        );
        assert!(third.accepted.is_empty());
        assert!(third.rejected[0].reason.contains("beyond the maxFiles"));
        assert_eq!(ledger.presented().len(), 3);
    }

    #[test]
    fn present_ceiling_caps_an_over_configured_budget() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("plain");
        std::fs::create_dir_all(&ws).unwrap();
        let ledger = ledger(&ws, &tmp.path().join("private"));
        let files: Vec<PresentedFile> = (0..MAX_PRESENT_FILES_CEILING + 5)
            .map(|i| PresentedFile {
                path: format!("f{i}.rs"),
                bytes: 1,
            })
            .collect();
        let outcome = ledger.present(&files, 1000);
        assert_eq!(outcome.accepted.len(), MAX_PRESENT_FILES_CEILING);
    }

    #[test]
    fn presented_files_appear_in_the_finalized_ledger() {
        let (_tmp, ws, private) = repo_fixture();
        std::fs::write(ws.join("out.md"), "report\n").unwrap();
        let ledger = ledger(&ws, &private);
        ledger.begin_turn(2);
        let described = ledger.describe(&ws.join("out.md")).unwrap();
        assert_eq!(described.path, "out.md");
        assert_eq!(described.bytes, 7);
        let outcome = ledger.present(&[described], DEFAULT_MAX_PRESENT_FILES);
        assert_eq!(outcome.accepted.len(), 1);

        let changes = ledger.finalize_turn(2).unwrap();
        assert_eq!(changes.presented.len(), 1);
        let rendered = changes.render();
        assert!(rendered.contains("out.md (7 B)"), "{rendered}");
        // Rendering a declaration carries the path, never the body.
        assert!(!rendered.contains("report\n"), "{rendered}");
    }

    #[test]
    fn changes_are_scoped_to_the_current_turn() {
        let (_tmp, ws, private) = repo_fixture();
        let ledger = ledger(&ws, &private);
        ledger.begin_turn(1);
        ledger.ensure_baseline().unwrap();
        std::fs::write(ws.join("one.txt"), "1\n").unwrap();
        assert_eq!(ledger.finalize_turn(1).unwrap().added.len(), 1);

        ledger.begin_turn(2);
        assert!(ledger.changes().unwrap().is_none(), "new turn starts clean");
        ledger.ensure_baseline().unwrap();
        std::fs::write(ws.join("two.txt"), "2\n").unwrap();
        let second = ledger.changes().unwrap().unwrap();
        assert_eq!(second.added.len(), 1);
        assert_eq!(second.added[0].path, "two.txt");
    }

    #[test]
    fn a_baseline_is_only_taken_inside_a_turn() {
        let (_tmp, ws, private) = repo_fixture();
        std::fs::write(ws.join("a.txt"), "one\n").unwrap();
        let ledger = ledger(&ws, &private);
        // No `begin_turn` yet: nothing to attribute, so nothing is captured.
        ledger.ensure_baseline().unwrap();
        assert!(!ledger.has_baseline());
        ledger.begin_turn(1);
        ledger.ensure_baseline().unwrap();
        assert!(ledger.has_baseline());
    }

    #[test]
    fn a_mid_turn_read_does_not_freeze_the_ledger() {
        let (_tmp, ws, private) = repo_fixture();
        let ledger = ledger(&ws, &private);
        ledger.begin_turn(1);
        ledger.ensure_baseline().unwrap();
        std::fs::write(ws.join("a.txt"), "a\n").unwrap();
        assert_eq!(ledger.changes().unwrap().unwrap().added.len(), 1);

        // A second change after a mid-turn read must still show up.
        std::fs::write(ws.join("b.txt"), "b\n").unwrap();
        assert_eq!(
            ledger.changes().unwrap().unwrap().added.len(),
            2,
            "a mid-turn read must not be cached as the turn's result"
        );

        // Once the turn is finalized, the stored ledger is served verbatim.
        let finalized = ledger.finalize_turn(1).unwrap();
        assert_eq!(finalized.added.len(), 2);
        assert_eq!(ledger.changes().unwrap().unwrap(), finalized);
    }

    #[test]
    fn human_bytes_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(2 * 1024 * 1024), "2.0 MiB");
    }

    #[test]
    fn budgets_from_env_default_and_override() {
        // One test: env vars are process-global (see .dev/AGENTS.md), and
        // `env_lock` is mandatory for anything that mutates them.
        let _guard = crate::test_util::env_lock();
        let saved = (
            std::env::var("RECURSIVE_DELIVERABLES_MAX_FILES").ok(),
            std::env::var("RECURSIVE_DELIVERABLES_MAX_FILE_BYTES").ok(),
            std::env::var("RECURSIVE_DELIVERABLES_COMPARE_MS").ok(),
            std::env::var("RECURSIVE_DELIVERABLES").ok(),
        );
        std::env::remove_var("RECURSIVE_DELIVERABLES_MAX_FILES");
        std::env::remove_var("RECURSIVE_DELIVERABLES_MAX_FILE_BYTES");
        std::env::remove_var("RECURSIVE_DELIVERABLES_COMPARE_MS");
        std::env::remove_var("RECURSIVE_DELIVERABLES");
        assert_eq!(Budgets::from_env(), Budgets::default());
        assert!(enabled_from_env());

        std::env::set_var("RECURSIVE_DELIVERABLES_MAX_FILES", "12");
        std::env::set_var("RECURSIVE_DELIVERABLES_MAX_FILE_BYTES", "4096");
        std::env::set_var("RECURSIVE_DELIVERABLES_COMPARE_MS", "5");
        let budgets = Budgets::from_env();
        assert_eq!(budgets.max_files, 12);
        assert_eq!(budgets.max_file_bytes, 4096);
        assert_eq!(budgets.compare_deadline_ms, 5);

        std::env::set_var("RECURSIVE_DELIVERABLES_MAX_FILES", "not-a-number");
        assert_eq!(Budgets::from_env().max_files, DEFAULT_MAX_FILES);

        std::env::set_var("RECURSIVE_DELIVERABLES", "0");
        assert!(!enabled_from_env());
        std::env::set_var("RECURSIVE_DELIVERABLES", "yes");
        assert!(enabled_from_env());

        for (key, value) in [
            ("RECURSIVE_DELIVERABLES_MAX_FILES", saved.0),
            ("RECURSIVE_DELIVERABLES_MAX_FILE_BYTES", saved.1),
            ("RECURSIVE_DELIVERABLES_COMPARE_MS", saved.2),
            ("RECURSIVE_DELIVERABLES", saved.3),
        ] {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}
