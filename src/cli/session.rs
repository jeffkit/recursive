//! Session management helpers: migrate, rewind, path resolution.

use std::path::{Path, PathBuf};

use anyhow::Context;
use recursive::{SessionFile, SessionWriter};

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
        1 => Ok(matches.into_iter().next().unwrap()),
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
    writer.finish("interrupted").ok();
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
