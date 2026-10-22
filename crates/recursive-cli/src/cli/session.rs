//! Session management helpers: migrate, rewind, path resolution.

use std::path::{Path, PathBuf};

use anyhow::Context;
use recursive::{SessionFile, SessionLock, SessionStatus, SessionWriter};

/// Implementation of `recursive migrate`.
pub(crate) fn cmd_migrate(workspace: &Path, dry_run: bool) -> anyhow::Result<()> {
    let report = recursive::migrate_workspace(workspace, dry_run)?;
    if report.already_clean {
        println!(
            "Workspace {} has no legacy in-tree state. Nothing to migrate.",
            workspace.display()
        );
        return Ok(());
    }
    let prefix = if dry_run { "(dry-run) " } else { "" };
    if !report.moved.is_empty() {
        println!("{prefix}Moved:");
        for (src, dst) in &report.moved {
            println!("  {} -> {}", src.display(), dst.display());
        }
    }
    if !report.skipped.is_empty() {
        println!("{prefix}Skipped (destination already exists):");
        for (src, dst) in &report.skipped {
            println!(
                "  {} stays put; {} already has data",
                src.display(),
                dst.display()
            );
        }
        eprintln!(
            "warning: some items were not migrated. Inspect the destinations and \
             merge manually if needed."
        );
    }
    if report.removed_empty_dotrecursive {
        println!("{prefix}Removed empty <workspace>/.recursive/");
    }
    Ok(())
}

/// Resolve a session path from a user-provided string.
///
/// If the string is an existing file or directory path, return it as-is.
/// Otherwise, search the workspace's session directory for a session whose
/// filename or directory name contains the given string (case-insensitive).
/// Returns an error if no match or multiple matches are found.
pub(crate) fn resolve_session_path(workspace: &Path, session: &str) -> anyhow::Result<PathBuf> {
    let path = PathBuf::from(session);

    // If it's an existing path, use it directly
    if path.exists() {
        return Ok(path);
    }

    // Search both the new (user data dir) and legacy (in-tree) session
    // directories so users with un-migrated state can still address
    // their old sessions.
    let new_dir = recursive::user_sessions_dir(workspace).ok();
    let legacy_dir = workspace.join(".recursive").join("sessions");
    let search_dirs: Vec<PathBuf> = new_dir
        .into_iter()
        .chain(if legacy_dir.is_dir() {
            Some(legacy_dir.clone())
        } else {
            None
        })
        .filter(|d| d.is_dir())
        .collect();

    if search_dirs.is_empty() {
        anyhow::bail!(
            "Session not found: '{}'. No sessions directory exists (looked in user data dir and {}).",
            session,
            legacy_dir.display()
        );
    }

    let lower = session.to_lowercase();
    let mut matches: Vec<PathBuf> = Vec::new();

    for sessions_dir in &search_dirs {
        // Search flat session files (old format: <timestamp>-<goal>.json)
        if let Ok(entries) = std::fs::read_dir(sessions_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_file() {
                    if let Some(name) = p.file_stem().and_then(|n| n.to_str()) {
                        if name.to_lowercase().contains(&lower) {
                            matches.push(p);
                        }
                    }
                }
            }
        }

        // Search nested session directories (new JSONL format)
        if let Ok(slug_entries) = std::fs::read_dir(sessions_dir) {
            for slug_entry in slug_entries.flatten() {
                let slug_dir = slug_entry.path();
                if !slug_dir.is_dir() {
                    continue;
                }
                if let Ok(session_entries) = std::fs::read_dir(&slug_dir) {
                    for session_entry in session_entries.flatten() {
                        let p = session_entry.path();
                        if p.is_dir() {
                            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                                if name.to_lowercase().contains(&lower) {
                                    matches.push(p);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    match matches.len() {
        0 => anyhow::bail!(
            "Session not found: '{}'. Use 'recursive sessions list' to see available sessions.",
            session
        ),
        1 => {
            let Some(path) = matches.into_iter().next() else {
                anyhow::bail!("internal error: session match vanished during resolution");
            };
            Ok(path)
        }
        n => {
            eprintln!("Multiple sessions match '{}' ({}):", session, n);
            for m in &matches {
                eprintln!("  {}", m.display());
            }
            anyhow::bail!("Ambiguous session identifier. Use a more specific path or ID.");
        }
    }
}

/// Implementation of `recursive sessions migrate-legacy`.
///
/// Reads a legacy single-file `.json` session (as written by
/// `--session-out`) and emits an equivalent JSONL session
/// directory under the user data dir, preserving the original
/// `tool_registry_hash`. The migrated session can then be resumed
/// by ID via `recursive resume <id>`.
pub(crate) fn cmd_session_migrate_legacy(workspace: &Path, path: &Path) -> anyhow::Result<()> {
    if !path.exists() {
        anyhow::bail!("legacy session file does not exist: {}", path.display());
    }
    let legacy = SessionFile::read_from(path)
        .with_context(|| format!("reading legacy session: {}", path.display()))?;

    // Open a fresh JSONL session, then patch in the carried-over hash.
    let mut writer = SessionWriter::create_with_tools(
        workspace,
        &legacy.goal,
        &legacy.model,
        &legacy.provider,
        &[],
        legacy.preset.as_deref(),
    )
    .with_context(|| "creating new JSONL session for migration")?;

    // Replay the legacy transcript through `append` (no filter —
    // we keep system messages for round-trip fidelity).
    for msg in legacy.messages() {
        writer.append(msg, None, None)?;
    }
    let session_dir = writer.session_dir().to_path_buf();
    writer.finish(SessionStatus::Interrupted).ok();
    drop(writer);

    // Patch `.meta.json` to carry over the legacy `tool_registry_hash`.
    let meta_path = session_dir.join(".meta.json");
    let bytes = std::fs::read(&meta_path)?;
    let mut meta: recursive::session::SessionMeta = serde_json::from_slice(&bytes)?;
    meta.tool_registry_hash = Some(legacy.tool_registry_hash.clone());
    std::fs::write(&meta_path, serde_json::to_string_pretty(&meta)?)?;

    println!("Migrated to: {}", session_dir.display());
    println!(
        "Resume with: recursive resume {}",
        session_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<id>"),
    );
    Ok(())
}

/// Implementation of `recursive sessions rewind`.
pub(crate) fn cmd_session_rewind(
    workspace: &Path,
    session: &str,
    to_turn: usize,
    force: bool,
    dry_run: bool,
) -> anyhow::Result<()> {
    let session_path = resolve_session_path(workspace, session)?;
    // The session path returned by resolve_session_path is the session
    // directory under .recursive/sessions/<slug>/<sid>/. The
    // checkpoints log lives inside it.
    if !session_path.is_dir() {
        anyhow::bail!(
            "Rewind requires a JSONL session directory; got file: {}",
            session_path.display()
        );
    }
    let log_path = session_path.join("checkpoints.jsonl");
    if !log_path.exists() {
        anyhow::bail!(
            "No checkpoints.jsonl in {}. \
             This session predates checkpointing or had it disabled.",
            session_path.display()
        );
    }

    // Issue #175: the rewind replaces transcript.jsonl and
    // checkpoints.jsonl by rename. An active writer holds an O_APPEND fd
    // on the old inode (SessionWriter takes the same sentinel lock, see
    // `SessionLock`), so renaming under it would strand every subsequent
    // message on the orphaned inode — silent data loss. Take the lock for
    // the whole operation and refuse while a live writer holds it; there
    // is deliberately no --force bypass.
    let _lock = SessionLock::acquire(&session_path)
        .map_err(|e| anyhow::anyhow!("cannot rewind session {}: {e}", session_path.display()))?;

    let plan = recursive::plan_rewind(&log_path, to_turn)?;

    println!("Rewind plan:");
    println!("  target checkpoint: {}", plan.target);
    println!("  turns to drop:     {:?}", plan.turns_to_drop);
    println!("  files to restore:  {} path(s)", plan.touched_paths.len());
    for p in &plan.touched_paths {
        println!("    - {p}");
    }
    if dry_run {
        println!("(--dry-run: not applied)");
        return Ok(());
    }

    let repo = recursive::ShadowRepo::open(workspace).map_err(|e| {
        anyhow::anyhow!(
            "cannot open shadow repo at {}/.recursive/shadow-git: {e}",
            workspace.display()
        )
    })?;

    let result = recursive::apply_rewind(&repo, &log_path, &plan, force)?;
    println!(
        "Rewind applied: {} restored, {} deleted, {} unchanged. {} turn(s) dropped from log.",
        result.stats.restored,
        result.stats.deleted,
        result.stats.unchanged,
        result.dropped_turns.len()
    );

    // Prune orphaned objects left by the rewind so the shadow repo
    // doesn't grow unboundedly across many rewind operations.
    if let Err(e) = repo.gc() {
        eprintln!("warning: shadow-git gc failed (non-fatal): {e}");
    }

    // Also truncate transcript.jsonl so the conversation state matches
    // the restored workspace state.
    match recursive::truncate_transcript_to_turn(&session_path, to_turn) {
        Ok(stats) => {
            println!(
                "Transcript truncated: {} message(s) kept, {} dropped.",
                stats.kept, stats.dropped
            );
        }
        Err(e) => {
            eprintln!("warning: transcript truncation failed: {e}");
        }
    }
    Ok(())
}

/// `recursive sessions gc-checkpoints` — run git gc on the shadow repo.
pub(crate) fn cmd_session_gc_checkpoints(workspace: &Path) -> anyhow::Result<()> {
    let repo = recursive::ShadowRepo::open(workspace)
        .map_err(|e| anyhow::anyhow!("cannot open shadow repo for {}: {e}", workspace.display()))?;
    println!("Running git gc on shadow repo (this may take a moment)…");
    repo.gc().map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("Done. Shadow repo pruned.");
    Ok(())
}

/// `recursive sessions clean-checkpoints` — delete the shadow repo entirely.
pub(crate) fn cmd_session_clean_checkpoints(workspace: &Path, force: bool) -> anyhow::Result<()> {
    let repo = recursive::ShadowRepo::open(workspace)
        .map_err(|e| anyhow::anyhow!("cannot open shadow repo for {}: {e}", workspace.display()))?;

    if !force {
        eprint!(
            "This will delete the shadow-git repo for {}.\n\
             Historical rewind for all sessions in this workspace will no longer work.\n\
             Continue? [y/N] ",
            workspace.display()
        );
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if input.trim().to_lowercase() != "y" {
            println!("Aborted.");
            return Ok(());
        }
    }

    repo.clean().map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("Shadow-git repo deleted. A fresh repo will be created on the next session.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::EnvGuard;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn has_git() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok()
    }

    /// Issue #222: `ShadowRepo::open` resolves the shadow-git dir through
    /// `RECURSIVE_HOME`, which sibling tests (`cli::resume`, `main.rs`) set
    /// and restore under the crate-wide [`EnvGuard`]. A rewind test that
    /// reads it unguarded can resolve one home while fabricating the
    /// session and another when `cmd_session_rewind` reopens the repo —
    /// the checkpoint sha then lands in a fresh bare repo ("checkpoint
    /// '<sha>' not found"), or in a sibling's already-deleted tempdir. Pin
    /// the home under the guard for the whole test; the guard also keeps
    /// the shadow repo out of the real `~/.recursive`.
    fn pinned_home() -> (TempDir, EnvGuard) {
        let home = TempDir::new().unwrap();
        let mut env = EnvGuard::lock();
        env.set_var("RECURSIVE_HOME", home.path());
        (home, env)
    }

    /// The fabricated session's shadow repo must live under the pinned
    /// home — proof the pin took effect before the rewind reopens it.
    fn assert_shadow_under(home: &TempDir) {
        assert!(
            home.path().join("workspaces").is_dir(),
            "shadow repo must be created under the pinned RECURSIVE_HOME {}",
            home.path().display()
        );
    }

    /// A minimal two-turn transcript. `truncate_transcript_to_turn` only
    /// peeks the `role` field, so bare user/assistant objects suffice.
    fn write_transcript(dir: &Path) {
        let mut lines = String::new();
        for t in 0..2 {
            for role in ["user", "assistant"] {
                let line = serde_json::json!({
                    "role": role,
                    "content": format!("{role} of turn {t}"),
                });
                lines.push_str(&line.to_string());
                lines.push('\n');
            }
        }
        fs::write(dir.join("transcript.jsonl"), lines).unwrap();
    }

    /// One `checkpoints.jsonl` record in the wire format `read_log` parses.
    fn append_log_record(dir: &Path, turn: usize, id: &str, pre: Option<&str>) {
        let rec = serde_json::json!({
            "turn": turn,
            "turn_id": turn,
            "id": id,
            "pre": pre,
            "touched_files": ["file.txt"],
            "touched_via": "structured",
            "saved_at": 0,
        });
        use std::io::Write as _;
        let mut f = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(dir.join("checkpoints.jsonl"))
            .unwrap();
        writeln!(f, "{rec}").unwrap();
    }

    /// A rewindable session backed by real shadow snapshots: two turns,
    /// each saved (`file.txt` "v1" after turn 0, "v2" after turn 1),
    /// plus a matching two-turn transcript. Returns the session dir.
    fn fabricate_saved_session(ws: &TempDir) -> PathBuf {
        let sess = ws.path().join("sess-01");
        fs::create_dir_all(&sess).unwrap();
        let file = ws.path().join("file.txt");
        fs::write(&file, "v0").unwrap();
        let repo = recursive::ShadowRepo::open(ws.path()).unwrap();
        let pre0 = repo.snapshot_for_session("sess-01", "t0 pre").unwrap();
        fs::write(&file, "v1").unwrap();
        let post0 = repo.snapshot_for_session("sess-01", "t0 post").unwrap();
        fs::write(&file, "v2").unwrap();
        let post1 = repo.snapshot_for_session("sess-01", "t1 post").unwrap();
        drop(repo);
        write_transcript(&sess);
        append_log_record(&sess, 0, &post0.0, Some(pre0.0.as_str()));
        append_log_record(&sess, 1, &post1.0, Some(post0.0.as_str()));
        sess
    }

    fn read_bytes(p: &Path) -> Vec<u8> {
        fs::read(p).unwrap()
    }

    fn assert_no_tmp_residue(dir: &Path) {
        for entry in fs::read_dir(dir).unwrap() {
            let name = entry.unwrap().file_name();
            let name = name.to_string_lossy();
            assert!(
                !name.starts_with(".tmp-") && !name.contains("rewind-tmp"),
                "no temp file may survive a completed rewind; found {name}"
            );
        }
    }

    /// Issue #175 (cluster acceptance 3a): while a live writer holds the
    /// session's `SessionLock`, `sessions rewind` must refuse — before
    /// touching the transcript or the checkpoint log — no matter what
    /// --force says. After the lock is released the same rewind must go
    /// through, proving the refusal came from the lock itself.
    #[test]
    fn rewind_refuses_while_session_locked() {
        if !has_git() {
            return;
        }
        let (home, _env) = pinned_home();
        let ws = TempDir::new().unwrap();
        let sess = fabricate_saved_session(&ws);
        assert_shadow_under(&home);
        let transcript_before = read_bytes(&sess.join("transcript.jsonl"));
        let log_before = read_bytes(&sess.join("checkpoints.jsonl"));

        let _held = SessionLock::acquire(&sess).unwrap();
        let err = cmd_session_rewind(ws.path(), sess.to_str().unwrap(), 1, true, false)
            .expect_err("rewind must refuse while the session lock is held");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("cannot rewind"),
            "error must name the refused operation; got: {msg}"
        );
        assert!(
            msg.contains("pid"),
            "busy error must say who holds the lock; got: {msg}"
        );
        assert_eq!(
            read_bytes(&sess.join("transcript.jsonl")),
            transcript_before,
            "refused rewind must not touch the transcript"
        );
        assert_eq!(
            read_bytes(&sess.join("checkpoints.jsonl")),
            log_before,
            "refused rewind must not touch the checkpoint log"
        );
        drop(_held);

        // Lock released: the identical command now succeeds, so the
        // refusal above was the lock, not the session shape.
        cmd_session_rewind(ws.path(), sess.to_str().unwrap(), 1, true, false).unwrap();
        assert_no_tmp_residue(&sess);
    }

    /// Issue #175 (cluster acceptance 3b): with no lock holder, the
    /// rewind replaces transcript and checkpoint log atomically — the
    /// on-disk files are complete and parseable, no temp residue — and a
    /// writer that opens the transcript *after* the rename appends to the
    /// new inode (its messages are not stranded on an orphaned file).
    #[test]
    fn rewind_replaces_transcript_atomically_and_appends_land_on_new_file() {
        if !has_git() {
            return;
        }
        let (home, _env) = pinned_home();
        let ws = TempDir::new().unwrap();
        let sess = fabricate_saved_session(&ws);
        assert_shadow_under(&home);

        cmd_session_rewind(ws.path(), sess.to_str().unwrap(), 1, true, false).unwrap();

        // Workspace restored to the start of turn 1.
        let file = ws.path().join("file.txt");
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "v1",
            "rewind --to-turn 1 must restore turn 0's post state"
        );
        // Transcript holds exactly turn 0 and is line-complete JSON.
        let transcript = fs::read_to_string(sess.join("transcript.jsonl")).unwrap();
        let lines: Vec<&str> = transcript.lines().collect();
        assert_eq!(lines.len(), 2, "only turn 0 survives: {transcript}");
        for line in &lines {
            serde_json::from_str::<serde_json::Value>(line)
                .unwrap_or_else(|e| panic!("transcript line must parse: {line}: {e}"));
        }
        // Checkpoint log truncated to the same cutoff.
        let log = fs::read_to_string(sess.join("checkpoints.jsonl")).unwrap();
        let kept: Vec<serde_json::Value> = log
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(kept.len(), 1, "only the turn-0 record survives: {log}");
        assert_eq!(kept[0]["turn"], 0);
        assert_no_tmp_residue(&sess);

        // Post-rewind writer: a fresh O_APPEND open (what SessionWriter
        // does after acquiring the lock) must land on the renamed file.
        use std::io::Write as _;
        let replay = serde_json::json!({"role":"user","content":"post-rewind turn"});
        fs::OpenOptions::new()
            .append(true)
            .open(sess.join("transcript.jsonl"))
            .unwrap()
            .write_all(format!("{replay}\n").as_bytes())
            .unwrap();
        let transcript = fs::read_to_string(sess.join("transcript.jsonl")).unwrap();
        let lines: Vec<&str> = transcript.lines().collect();
        assert_eq!(lines.len(), 3, "appended message must reach the live file");
        let last: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(last["content"], "post-rewind turn");
        assert_no_tmp_residue(&sess);
    }

    /// Issue #223: the nested-directory scan used to carry a `delete !`
    /// mutant on `if !slug_dir.is_dir()` that inverted the filter — real
    /// session dirs got skipped, and JSONL sessions became unaddressable
    /// by ID while every test stayed green. Pin both stored layouts plus
    /// the miss and ambiguity branches.
    #[test]
    fn resolve_session_path_finds_flat_files_and_nested_session_dirs() {
        let (home, mut env) = pinned_home();
        // `RECURSIVE_SESSIONS_DIR` hard-overrides the user dir this function
        // searches; clear any ambient value so both writers agree on the root.
        env.remove_var("RECURSIVE_SESSIONS_DIR");
        let ws = TempDir::new().unwrap();
        let sessions = recursive::user_sessions_dir(ws.path()).unwrap();
        let flat = sessions.join("20260101-legacy-goal.json");
        let nested = sessions.join("proj-x").join("sess-alpha-01");
        let ambiguous = sessions.join("proj-y").join("sess-alpha-02");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(&ambiguous).unwrap();
        fs::write(&flat, "{}").unwrap();
        // A plain file between the slug dirs: the slug scan must skip it via
        // the `!is_dir` guard, not descend into it.
        fs::write(sessions.join("notes.txt"), "not a session").unwrap();

        assert_eq!(
            resolve_session_path(ws.path(), "legacy-goal").unwrap(),
            flat,
            "flat single-file sessions match by file stem"
        );
        assert_eq!(
            resolve_session_path(ws.path(), "alpha-01").unwrap(),
            nested,
            "JSONL session dirs match by directory name"
        );
        assert_eq!(
            resolve_session_path(ws.path(), "ALPHA-01").unwrap(),
            nested,
            "matching is case-insensitive"
        );
        let miss = resolve_session_path(ws.path(), "no-such-session")
            .expect_err("an unknown id must be an error");
        assert!(
            format!("{miss:#}").contains("Session not found"),
            "miss must say so; got: {miss:#}"
        );
        let ambiguous_err = resolve_session_path(ws.path(), "alpha-0")
            .expect_err("two substring hits must refuse to guess");
        assert!(
            format!("{ambiguous_err:#}").contains("Ambiguous session identifier"),
            "ambiguity must refuse to guess; got: {ambiguous_err:#}"
        );
        assert_shadow_under(&home);
    }

    /// Issue #223: `sessions migrate-legacy` had zero coverage — the body
    /// could return `Ok(())` without creating anything, or refuse existing
    /// files (inverted `!path.exists()`), with no test noticing. Pin the
    /// full round trip: replayed transcript, carried-over tool registry
    /// hash, and the missing-file refusal.
    #[test]
    fn migrate_legacy_replays_transcript_and_carries_the_registry_hash() {
        let (home, mut env) = pinned_home();
        env.remove_var("RECURSIVE_SESSIONS_DIR");
        let ws = TempDir::new().unwrap();
        let legacy_path = ws.path().join("legacy-session.json");
        let legacy = recursive::session::SessionFile {
            schema_version: 1,
            goal: "migrate me".into(),
            model: "test-model".into(),
            provider: "test-provider".into(),
            tool_registry_hash: "hash-legacy".into(),
            steps_consumed: 3,
            transcript: vec![
                recursive::Message::user("hello"),
                recursive::Message::assistant("hi"),
            ],
            preset: None,
        };
        legacy.write_to(&legacy_path).unwrap();

        cmd_session_migrate_legacy(ws.path(), &legacy_path).unwrap();

        // Exactly one JSONL session dir appeared under the user sessions root.
        let sessions = recursive::user_sessions_dir(ws.path()).unwrap();
        let created: Vec<PathBuf> = fs::read_dir(&sessions)
            .unwrap()
            .flatten()
            .map(|slug| slug.path())
            .filter(|p| p.is_dir())
            .flat_map(|slug| {
                fs::read_dir(&slug)
                    .unwrap()
                    .flatten()
                    .map(|s| s.path())
                    .filter(|p| p.is_dir())
                    .collect::<Vec<_>>()
            })
            .filter(|p| p.join(".meta.json").exists())
            .collect();
        assert_eq!(
            created.len(),
            1,
            "migrate must create exactly one session dir under {sessions:?}: {created:?}"
        );
        let sess = &created[0];
        // The transcript was replayed line for line.
        let transcript = fs::read_to_string(sess.join("transcript.jsonl")).unwrap();
        let lines: Vec<&str> = transcript.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "both legacy messages must be replayed: {transcript}"
        );
        // The legacy tool_registry_hash survived the migration (.meta.json).
        let meta: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(sess.join(".meta.json")).unwrap()).unwrap();
        assert_eq!(
            meta["tool_registry_hash"], "hash-legacy",
            "the carried-over hash is the point of the migration: {meta}"
        );

        // A missing legacy file is refused before anything is created.
        let absent = ws.path().join("absent.json");
        let err = cmd_session_migrate_legacy(ws.path(), &absent)
            .expect_err("a nonexistent legacy file must be refused");
        assert!(
            format!("{err:#}").contains("does not exist"),
            "refusal must name the problem; got: {err:#}"
        );
        assert_shadow_under(&home);
    }
}
