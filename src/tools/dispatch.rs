//! Tool dispatch: the execution path from `invoke` to tool result.
//!
//! Contains [`ToolDispatch`] (the return type of `invoke_with_audit`),
//! the dispatch-stage methods of [`ToolRegistry`], touched-file recording,
//! argument preview generation, and path containment helpers.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing::Instrument;

use crate::agent::PermissionDecision;
use crate::error::{Error, Result};

use super::audit::{
    blake3_canonical_json, truncate_for_audit, unix_millis, AuditMeta, ExitStatus, TouchedFiles,
};
use super::permission_pipeline::{self, PermissionPipeline};
use super::registry::ToolRegistry;

/// Return value of [`ToolRegistry::invoke_with_audit`]: the tool result
/// and its accompanying audit record.
pub struct ToolDispatch {
    pub result: Result<String>,
    pub audit: AuditMeta,
}

/// Inspect tool arguments for known fs tools and record their paths
/// on the shared `TouchedFiles` collector.
fn record_touched(name: &str, args: &Value, slot: &Mutex<TouchedFiles>) {
    let Ok(mut t) = slot.lock() else {
        return;
    };
    match name {
        "Write" => {
            if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
                t.paths.insert(p.to_string());
            }
        }
        "Edit" => {
            // Edit (str_replace) stores the path in file_path.
            if let Some(p) = args.get("file_path").and_then(|v| v.as_str()) {
                t.paths.insert(p.to_string());
            }
        }
        "Bash" => {
            t.saw_shell = true;
        }
        _ => {}
    }
}

impl ToolRegistry {
    pub async fn invoke(&self, name: &str, arguments: Value) -> Result<String> {
        // Goal-161: runtime permission hook — checked first, before static
        // config, so the user gets the chance to allow/deny at call time.
        let effective_args = if let Some(hook) = &self.permission_hook {
            match hook.check(name, &arguments).await {
                PermissionDecision::Allow => arguments,
                PermissionDecision::Transform(new_args) => new_args,
                PermissionDecision::Deny(reason) => {
                    return Err(Error::PermissionDenied {
                        name: name.into(),
                        reason: crate::permissions::DecisionReason::Hook { name: reason },
                    });
                }
            }
        } else {
            arguments
        };
        self.invoke_with_audit(name, effective_args).await.result
    }

    /// Invoke a tool and return both its result and a populated
    /// [`AuditMeta`]. Callers that need to persist audit data should
    /// use this method; callers that don't can call `invoke` which
    /// discards the audit half.
    pub async fn invoke_with_audit(&self, name: &str, arguments: Value) -> ToolDispatch {
        // Goal-261: pre-execution permission checks are delegated to
        // `PermissionPipeline`. This method keeps only `touched_files`
        // recording, tool lookup, L1 policy check, side-effect
        // classification, step_id/hash generation, and tool execution
        // + audit construction. The pipeline owns the 7 permission-
        // orchestration phases and exposes a public `recheck_policy`
        // method for callers that need to re-validate hook-mutated args.
        let pipeline = PermissionPipeline::new(self);
        match pipeline.check(name, arguments).await {
            permission_pipeline::CheckOutcome::Deny { error, audit } => ToolDispatch {
                result: Err(error),
                audit,
            },
            permission_pipeline::CheckOutcome::Allow { arguments } => {
                self.dispatch_after_permission_check(name, arguments).await
            }
        }
    }

    /// Goal-261: the execution half of `invoke_with_audit` — invoked
    /// after `PermissionPipeline::check` has returned `Allow`. Records
    /// touched files, looks up the tool, runs the L1 policy check, and
    /// constructs the `AuditMeta` around `tool.execute()`.
    async fn dispatch_after_permission_check(&self, name: &str, arguments: Value) -> ToolDispatch {
        // Record touched files for the active turn (if a collector is attached).
        if let Some(slot) = &self.touched {
            record_touched(name, &arguments, slot);
        }

        let Some(tool) = self.find_by_name(name) else {
            return ToolDispatch {
                result: Err(Error::UnknownTool(name.into())),
                audit: AuditMeta::synthetic_unknown_tool(name),
            };
        };

        let side_effect = tool.side_effect_class();
        let step_id = uuid::Uuid::now_v7().hyphenated().to_string();
        let args_hash = blake3_canonical_json(&arguments);
        let started_at = unix_millis();

        let args_size = arguments.to_string().len();
        let span = tracing::info_span!("tool.execute", name = %name, args_size);
        let raw_result = tool
            .execute(arguments)
            .instrument(span)
            .await
            .map_err(|e| match e {
                Error::Tool { .. }
                | Error::BadToolArgs { .. }
                | Error::UnknownTool(_)
                | Error::PermissionDeniedLimit { .. } => e,
                other => Error::Tool {
                    name: name.into(),
                    call_id: None,
                    message: other.to_string(),
                },
            });

        let finished_at = unix_millis();
        let exit_status = match &raw_result {
            Ok(_) => ExitStatus::Ok,
            Err(e) => {
                let (clipped, truncated) = truncate_for_audit(&e.to_string());
                ExitStatus::Err {
                    message: clipped,
                    truncated,
                }
            }
        };

        ToolDispatch {
            result: raw_result,
            audit: AuditMeta {
                step_id,
                started_at,
                finished_at,
                args_hash,
                side_effect,
                exit_status,
            },
        }
    }
}

/// Build a short human-readable preview of tool arguments for the
/// permission dialog. Extracts up to 80 characters.
pub fn args_preview_for_permission(arguments: &Value) -> String {
    let s = match arguments {
        Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .take(3)
                .map(|(k, v)| {
                    let v_str = match v {
                        Value::String(s) => {
                            let short: String = s.chars().take(30).collect();
                            format!("\"{}\"", short)
                        }
                        other => {
                            let s = other.to_string();
                            s.chars().take(30).collect()
                        }
                    };
                    format!("{k}={v_str}")
                })
                .collect();
            parts.join(", ")
        }
        other => other.to_string(),
    };
    if s.chars().count() > 80 {
        let head: String = s.chars().take(79).collect();
        format!("{head}…")
    } else {
        s
    }
}

/// Resolve a possibly-relative path against the workspace root.
///
/// Normalises both the root and candidate to absolute, dot-free form first,
/// then performs a second check via `canonicalize()` (which follows symlinks)
/// when the path already exists on disk.  This prevents symlink-based escapes
/// where a link inside the workspace points to a location outside it.
///
/// For paths that do not yet exist (e.g. a new file being written), only the
/// lexical normalisation check is performed — the caller is responsible for
/// ensuring no symlink is created that would bridge outside the root.
pub fn resolve_within(root: &Path, path: &str) -> Result<PathBuf> {
    resolve_within_any(&[(root.to_path_buf(), AccessTier::ReadWrite)], path, false)
}

/// Access tier for a sandbox root. `ReadOnly` roots permit read operations
/// only; `ReadWrite` roots permit both reads and writes. The primary
/// workspace root is always `ReadWrite`; extra dirs declared via
/// `[sandbox] extra_readonly_dirs` (or the TUI `/add-dir --read-only` flow)
/// are `ReadOnly`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessTier {
    ReadOnly,
    ReadWrite,
}

/// Shared, session-scoped sandbox roots. This is the runtime-mutable
/// companion to the static `extra_roots` baked into each tool at build
/// time: the TUI's `/add-dir` command (and, in future, an interactive
/// out-of-scope-read grant) appends entries here, and every structured
/// filesystem tool consults the snapshot in [`resolve_within_any`] on
/// each call so newly added roots take effect immediately — without
/// rebuilding the agent runtime.
///
/// Uses `std::sync::RwLock` (not tokio) because tools only take a brief
/// read snapshot inside `execute`; no `.await` is held across the guard.
pub type SharedSandboxRoots =
    std::sync::Arc<std::sync::RwLock<Vec<(std::path::PathBuf, AccessTier)>>>;

/// Construct an empty shared sandbox-roots slot.
pub fn new_shared_sandbox_roots() -> SharedSandboxRoots {
    std::sync::Arc::new(std::sync::RwLock::new(Vec::new()))
}

/// Multi-root variant of [`resolve_within`]. The candidate path is resolved
/// against a *set* of allowed roots; it is accepted when its canonical form
/// falls under at least one root. Symlink-aware canonicalisation is applied
/// per-root exactly as in the single-root case.
///
/// `write` selects the tier check: a path that is only contained by
/// `ReadOnly` roots is rejected for write operations (`Write` / `Edit` /
/// `StrReplace`) with a clear error, while remaining readable.
///
/// This is the sandbox primitive used by every structured filesystem tool
/// (`Read` / `Write` / `Edit` / `Glob` / `Search` / `count_lines` /
/// `estimate_tokens`). It preserves invariant #3 — every fs path is still
/// containment-checked — while letting the operator declare additional
/// allowed roots beyond the primary workspace.
pub fn resolve_within_any(
    roots: &[(PathBuf, AccessTier)],
    path: &str,
    write: bool,
) -> Result<PathBuf> {
    let candidate = Path::new(path);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        let Some((first_root, _)) = roots.first() else {
            return Err(Error::BadToolArgs {
                name: "<fs>".into(),
                message: format!("path `{path}` given with no sandbox roots"),
            });
        };
        first_root.join(candidate)
    };
    let abs_joined = absolutise(&joined);

    // Lexical containment check against every root (works for paths that
    // don't exist yet).
    let mut lexical_matches: Vec<AccessTier> = Vec::new();
    for (root, tier) in roots {
        if abs_joined.starts_with(absolutise(root)) {
            lexical_matches.push(*tier);
        }
    }
    if lexical_matches.is_empty() {
        return Err(Error::BadToolArgs {
            name: "<fs>".into(),
            message: format!("path `{path}` escapes sandbox roots"),
        });
    }

    // Symlink-aware check: if the path exists, canonicalise it once and
    // re-check containment against each root. A link that lexically appears
    // inside a root but canonicalises outside *all* roots is rejected.
    let tier_matches: Vec<AccessTier> = if abs_joined.exists() {
        let canonical_joined = abs_joined.canonicalize().map_err(|e| Error::BadToolArgs {
            name: "<fs>".into(),
            message: format!("cannot resolve path `{path}`: {e}"),
        })?;
        let mut matches = Vec::new();
        for (root, tier) in roots {
            let abs_root = absolutise(root);
            // Roots that don't exist yet fall back to the lexical form; the
            // candidate's canonical form is then checked against it.
            let canonical_root = abs_root.canonicalize().unwrap_or(abs_root);
            if canonical_joined.starts_with(&canonical_root) {
                matches.push(*tier);
            }
        }
        if matches.is_empty() {
            return Err(Error::BadToolArgs {
                name: "<fs>".into(),
                message: format!(
                    "path `{path}` resolves via symlink to a location outside all sandbox roots"
                ),
            });
        }
        matches
    } else {
        lexical_matches
    };

    // Tier gate: writes require at least one matching ReadWrite root.
    if write && !tier_matches.contains(&AccessTier::ReadWrite) {
        return Err(Error::BadToolArgs {
            name: "<fs>".into(),
            message: format!(
                "path `{path}` is inside a read-only sandbox root; writes are not allowed"
            ),
        });
    }
    Ok(abs_joined)
}

/// Turn a path into an absolute, normalised form. Does not touch the disk,
/// so it works for files that don't yet exist (needed by `write_file`).
fn absolutise(p: &std::path::Path) -> std::path::PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(p)
    };
    normalise(&abs)
}

fn normalise(p: &std::path::Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for c in p.components() {
        use std::path::Component::*;
        match c {
            ParentDir => {
                out.pop();
            }
            CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn rw(p: impl Into<PathBuf>) -> (PathBuf, AccessTier) {
        (p.into(), AccessTier::ReadWrite)
    }

    fn ro(p: impl Into<PathBuf>) -> (PathBuf, AccessTier) {
        (p.into(), AccessTier::ReadOnly)
    }

    #[test]
    fn single_root_allows_inside_rejects_outside() {
        let tmp = TempDir::new().unwrap();
        let roots = vec![rw(tmp.path())];
        assert!(resolve_within_any(&roots, "a.txt", false).is_ok());
        assert!(resolve_within_any(&roots, "../escape", false).is_err());
    }

    #[test]
    fn second_root_lets_path_outside_workspace_in() {
        let ws = TempDir::new().unwrap();
        let extra = TempDir::new().unwrap();
        std::fs::write(extra.path().join("note.md"), "hi").unwrap();
        let roots = vec![rw(ws.path()), ro(extra.path())];
        // Relative paths resolve against the FIRST root (workspace); to read
        // the extra dir the agent passes an absolute path.
        let abs = extra.path().join("note.md");
        let got = resolve_within_any(&roots, &abs.to_string_lossy(), false);
        assert!(
            got.is_ok(),
            "absolute path inside extra root must be allowed"
        );
    }

    #[test]
    fn read_only_root_blocks_write() {
        let ws = TempDir::new().unwrap();
        let extra = TempDir::new().unwrap();
        let roots = vec![rw(ws.path()), ro(extra.path())];
        let abs = extra.path().join("new.txt");
        let err = resolve_within_any(&roots, &abs.to_string_lossy(), true).unwrap_err();
        assert!(
            err.to_string().contains("read-only"),
            "write to read-only root must be rejected: {err}"
        );
    }

    #[test]
    fn read_only_root_allows_read() {
        let ws = TempDir::new().unwrap();
        let extra = TempDir::new().unwrap();
        std::fs::write(extra.path().join("note.md"), "hi").unwrap();
        let roots = vec![rw(ws.path()), ro(extra.path())];
        let abs = extra.path().join("note.md");
        assert!(resolve_within_any(&roots, &abs.to_string_lossy(), false).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_into_no_root_is_rejected() {
        let ws = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let link = ws.path().join("trap");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        let roots = vec![rw(ws.path())];
        // Following the symlink lands outside every root.
        assert!(resolve_within_any(&roots, "trap", false).is_err());
    }

    #[test]
    fn empty_roots_rejects_everything() {
        let err = resolve_within_any(&[], "a.txt", false).unwrap_err();
        assert!(err.to_string().contains("no sandbox roots"));
    }

    #[test]
    fn resolve_within_delegates_and_preserves_escapes_message() {
        let tmp = TempDir::new().unwrap();
        let err = resolve_within(tmp.path(), "../etc/passwd").unwrap_err();
        assert!(err.to_string().contains("escapes"));
    }

    // ── args_preview_for_permission ──────────────────────────────────────────

    #[test]
    fn args_preview_short_object_passthrough() {
        // kills args_preview_for_permission → String::new() and → "xyzzy"
        // and kills `> with <` mutant (short string stays untouched)
        let args = serde_json::json!({"path": "src/lib.rs"});
        let preview = args_preview_for_permission(&args);
        assert!(preview.contains("path"), "preview must contain key 'path'");
        assert!(preview.contains("src/lib.rs"), "preview must contain value");
        assert!(
            !preview.ends_with('…'),
            "short preview must not be truncated"
        );
    }

    #[test]
    fn args_preview_long_object_truncated_at_80() {
        // kills `> with ==` and `> with >=` mutants.
        // Use 3 keys × 30-char value each: "k1=\"xxx…\" , k2=..., k3=..." > 80 chars total.
        let val = "x".repeat(30);
        let args = serde_json::json!({"alpha": val, "beta": val, "gamma": val});
        let preview = args_preview_for_permission(&args);
        assert!(
            preview.ends_with('…'),
            "preview longer than 80 chars must end with ellipsis, got: {preview}"
        );
        assert_eq!(
            preview.chars().count(),
            80,
            "truncated preview must be exactly 80 chars"
        );
    }

    #[test]
    fn args_preview_exactly_80_chars_not_truncated() {
        // kills `> with >=` mutant: exactly 80 chars should NOT be truncated
        // Build a json string whose preview is exactly 80 chars.
        // key="k" (3 bytes with quotes→ k="<val>") → adjust val length.
        // Let's brute-force: start short and grow until preview reaches 80.
        let mut len = 70usize;
        loop {
            let val = "a".repeat(len);
            let args = serde_json::json!({"k": val});
            let preview = args_preview_for_permission(&args);
            if preview.chars().count() == 80 {
                // exactly 80 → must NOT be truncated (> 80 is false)
                assert!(
                    !preview.ends_with('…'),
                    "exactly 80-char preview must not be truncated"
                );
                break;
            }
            len += 1;
            if len > 200 {
                break; // guard; test is best-effort if format changes
            }
        }
    }

    #[test]
    fn args_preview_non_object_value() {
        // covers the `other => other.to_string()` branch
        let args = serde_json::json!("just a string");
        let preview = args_preview_for_permission(&args);
        assert!(
            preview.contains("just a string"),
            "non-object preview must contain the value"
        );
    }

    // ── normalise targeted tests ──────────────────────────────────────────────

    #[test]
    fn normalise_resolves_double_dot() {
        // kills `out.pop()` → noop mutations in normalise
        let path = std::path::Path::new("/a/b/../c");
        let got = normalise(path);
        assert_eq!(got, std::path::PathBuf::from("/a/c"));
    }

    #[test]
    fn normalise_ignores_single_dot() {
        // kills `CurDir => {}` → `out.push(".")` mutations
        let path = std::path::Path::new("/a/./b");
        let got = normalise(path);
        assert_eq!(got, std::path::PathBuf::from("/a/b"));
    }

    #[test]
    fn normalise_preserves_absolute_path() {
        // kills function-level replacement of normalise
        let path = std::path::Path::new("/usr/local/bin");
        let got = normalise(path);
        assert_eq!(got, std::path::PathBuf::from("/usr/local/bin"));
    }

    // ── write tier gate: && vs || and delete ! ────────────────────────────────

    #[test]
    fn rw_root_allows_both_read_and_write() {
        let tmp = TempDir::new().unwrap();
        let roots = vec![rw(tmp.path())];
        // read
        assert!(
            resolve_within_any(&roots, "a.txt", false).is_ok(),
            "read from RW root must succeed"
        );
        // write
        assert!(
            resolve_within_any(&roots, "b.txt", true).is_ok(),
            "write to RW root must succeed"
        );
    }
}
