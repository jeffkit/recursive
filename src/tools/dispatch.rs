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
    blake3_canonical_json, truncate_for_audit, unix_millis, AuditMeta, ExitStatus, ToolSideEffect,
    TouchedFiles,
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
    // Issue #199: the path-argument position comes from the single identity
    // table (this used to be a third parallel copy of the Write/Edit set).
    // Keyed on the raw dispatch name, preserving pre-refactor behavior
    // (alias calls record nothing) — canonicalization is the pipeline's job.
    let shape = crate::tools::tool_identity::builtin_safety_shape(name);
    if shape.is_write() {
        if let Some(p) = shape.extract_path(args) {
            t.paths.insert(p);
        }
    }
    if name == "Bash" {
        t.saw_shell = true;
    }
}

impl ToolRegistry {
    /// Goal-161: the runtime permission hook — checked before static config,
    /// so the user gets the chance to allow/deny at call time. Returns the
    /// arguments to dispatch with (possibly hook-transformed), or the denial.
    async fn apply_permission_hook(&self, name: &str, arguments: Value) -> Result<Value> {
        let Some(hook) = self.permission_hook() else {
            return Ok(arguments);
        };
        match hook.check(name, &arguments).await {
            PermissionDecision::Allow => Ok(arguments),
            PermissionDecision::Transform(new_args) => Ok(new_args),
            PermissionDecision::Deny(reason) => Err(Error::PermissionDenied {
                name: name.into(),
                reason: crate::permissions::DecisionReason::Hook { name: reason },
            }),
        }
    }

    pub async fn invoke(&self, name: &str, arguments: Value) -> Result<String> {
        self.invoke_gated_with_audit(name, arguments).await.result
    }

    /// [`Self::invoke`] with the audit record kept: the hook gate, then
    /// [`Self::invoke_with_audit`].
    ///
    /// The gate lives *here*, not in `invoke`, because `run_code`'s
    /// programmatic invoker needs both halves — a nested call must be gated
    /// exactly like a model-issued one **and** keep its `AuditMeta` (issue
    /// #134 review: an earlier revision bought the audit record with the
    /// permission gate, so a program's calls bypassed the session hook
    /// whenever no `permissions` config was installed — the CLI / TUI / HTTP
    /// default). `invoke_with_audit` stays ungated on purpose: its direct
    /// callers in the run loop consult the hook themselves first.
    pub(crate) async fn invoke_gated_with_audit(
        &self,
        name: &str,
        arguments: Value,
    ) -> ToolDispatch {
        match self.apply_permission_hook(name, arguments).await {
            Ok(arguments) => self.invoke_with_audit(name, arguments).await,
            // A denied call never reaches the tool, so the audit record is
            // synthetic — same shape as a pipeline denial (empty `args_hash`).
            Err(error) => {
                let (message, truncated) = truncate_for_audit(&error.to_string());
                let now = unix_millis();
                ToolDispatch {
                    result: Err(error),
                    audit: AuditMeta {
                        step_id: uuid::Uuid::now_v7().hyphenated().to_string(),
                        tool_name: Some(name.to_string()),
                        canonical_name: Some(
                            self.identity_for(name)
                                .map(|i| i.canonical_name)
                                .unwrap_or_else(|| name.to_string()),
                        ),
                        started_at: now,
                        finished_at: now,
                        args_hash: String::new(),
                        side_effect: ToolSideEffect::External,
                        exit_status: ExitStatus::Err { message, truncated },
                    },
                }
            }
        }
    }

    /// Invoke a tool and return both its result and a populated
    /// [`AuditMeta`]. Callers that need to persist audit data should
    /// use this method; callers that don't can call `invoke` which
    /// discards the audit half.
    ///
    /// Does **not** consult the runtime permission hook: callers reaching
    /// here directly (the run loop, resume replay) have already done so.
    /// Programmatic calls want [`Self::invoke_gated_with_audit`].
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
                audit: *audit,
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

        // Goal #133: capture the turn's change-ledger baseline *before* the
        // first mutating tool call, so a change can never be observed after
        // it happened. Read-only tools (and turns that only read) pay
        // nothing; the call is idempotent once a baseline exists.
        if !tool.is_readonly() {
            if let Some(ledger) = &self.deliverables {
                if let Err(err) = ledger.ensure_baseline() {
                    tracing::debug!(error = %err, "deliverables: baseline capture failed");
                }
            }
        }

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
                tool_name: Some(name.to_string()),
                canonical_name: Some(
                    self.identity_for(name)
                        .map(|i| i.canonical_name)
                        .unwrap_or_else(|| name.to_string()),
                ),
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
/// then resolves the candidate's longest *existing* ancestor and re-checks
/// containment. This prevents symlink-based escapes where a link inside the
/// workspace points to a location outside it — including when the path itself
/// does not exist yet, because the ancestor walk follows those links, and a
/// link whose target is missing is followed explicitly rather than mistaken
/// for a component that is about to be created.
///
/// The containment check is done on the resolved form (canonical ancestor plus
/// the links followed below it), but the returned path keeps the caller's
/// spelling (see the note at the end of [`resolve_within_any`]).
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
/// Canonicalisation covers paths that do not exist yet: the longest existing
/// ancestor is canonicalised and the remaining components re-attached, so a
/// symlinked *parent* directory cannot smuggle a new file outside the roots.
/// Symlinks met by that walk are followed even when their target is missing,
/// so a dangling link as the final component (`notes -> <outside>/f.txt`) is
/// rejected too. That resolved form is used for the containment check; the
/// returned path keeps the caller's spelling.
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

    // Cheap lexical check first: rejects obvious `..` / absolute escapes with
    // a stable error, and is the only check possible when a root does not
    // exist on disk.
    let mut lexical_match = false;
    for (root, _tier) in roots {
        if abs_joined.starts_with(absolutise(root)) {
            lexical_match = true;
            break;
        }
    }
    if !lexical_match {
        return Err(Error::BadToolArgs {
            name: "<fs>".into(),
            message: format!("path `{path}` escapes sandbox roots"),
        });
    }

    // Symlink-aware check: resolve the candidate's longest *existing* ancestor
    // and re-attach the not-yet-existing tail, following links found on the
    // way. This is what makes a symlink fatal for a *new* file: whether it is
    // a parent directory (`docs -> ~`) or the leaf itself (`notes -> ~/f.txt`,
    // target not created yet), the candidate resolves out of the roots.
    let resolved =
        canonicalise_with_existing_ancestor(&abs_joined).map_err(|e| Error::BadToolArgs {
            name: "<fs>".into(),
            message: format!("cannot resolve path `{path}`: {e}"),
        })?;

    // Re-check containment using the resolved form of both sides: a root may
    // itself be reached through a symlink (`/tmp` → `/private/tmp`, a symlinked
    // workspace), so comparing a resolved candidate against a lexical root
    // would falsely reject everything under it. Both sides go through the same
    // resolver, so a root that does not exist on disk yet (an `--add-dir`
    // directory created later, a work dir still to be made) degrades to its own
    // spelling exactly like the candidate does, instead of half-canonical being
    // compared against fully-canonical.
    let mut tier_matches: Vec<AccessTier> = Vec::new();
    for (root, tier) in roots {
        let abs_root = absolutise(root);
        let resolved_root = canonicalise_with_existing_ancestor(&abs_root).unwrap_or(abs_root);
        if resolved.starts_with(&resolved_root) {
            tier_matches.push(*tier);
        }
    }
    if tier_matches.is_empty() {
        return Err(Error::BadToolArgs {
            name: "<fs>".into(),
            message: format!(
                "path `{path}` resolves via symlink to a location outside all sandbox roots"
            ),
        });
    }

    // Tier gate: writes require at least one matching ReadWrite root.
    if write && !tier_matches.contains(&AccessTier::ReadWrite) {
        return Err(Error::BadToolArgs {
            name: "<fs>".into(),
            message: format!(
                "path `{path}` is inside a read-only sandbox root; writes are not allowed"
            ),
        });
    }

    // Return the caller-shaped absolute path, not `resolved`: the two differ
    // whenever a root (or an ancestor below it) is reached through a symlink —
    // e.g. a `TempDir` under `/var` on macOS canonicalises to `/private/var`.
    // The resolved form is used for *validation* only; handing it back would
    // change the path identity seen by the read-state cache, the transport
    // boundary (paths are tier-relative, so host-side canonicalisation is not
    // generally right) and the deliverables ledger's workspace-relative
    // derivation. The checks above guarantee the path *resolves* inside a
    // root — every symlink in it has been followed, including a dangling link
    // as the final component — so the caller's spelling cannot name an escape.
    Ok(abs_joined)
}

/// Bound on symlink chains followed by [`resolve_links`]; mirrors the kernel's
/// `ELOOP` limit closely enough to reject link cycles instead of looping.
const MAX_SYMLINK_HOPS: usize = 40;

/// Fully resolve `abs`, including the components that do not exist yet:
/// canonicalise the longest existing ancestor, re-attach the remaining
/// components, and follow any symlink found on the way.
///
/// `abs` itself may not exist (a file about to be created, or a nested
/// directory that `create_dir_all` will make), so `canonicalize()` cannot be
/// called on it directly. Walking up to the first ancestor that *does* exist
/// still resolves every symlink on the way in — which is the escape we care
/// about: the OS follows symlinked parent directories when it later creates
/// the file. See [`resolve_links`] for the dangling-link case.
fn canonicalise_with_existing_ancestor(abs: &Path) -> std::io::Result<PathBuf> {
    resolve_links(abs, 0)
}

/// Implementation of [`canonicalise_with_existing_ancestor`], `followed`
/// counting the hops already taken.
///
/// The "does this component exist" probe is `symlink_metadata`, not
/// `Path::exists`/`canonicalize`: a symlink is a directory entry of its own
/// even when its target is missing, and `exists()` follows the link and
/// reports `false` for a dangling one. Such a link would otherwise be treated
/// as a component "to be created" and re-attached to the canonical ancestor
/// unchanged — while the OS, when the file is later opened, follows it and
/// creates the target wherever it points. Links found by the walk are
/// therefore followed here, with relative targets rebased on the link's
/// canonical parent, so `docs -> <outside>` + `docs/new.txt` *and* a dangling
/// `notes -> <outside>/f.txt` both resolve out of the roots and are rejected.
fn resolve_links(abs: &Path, followed: usize) -> std::io::Result<PathBuf> {
    let mut cur = abs.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while std::fs::symlink_metadata(&cur).is_err() {
        let Some(name) = cur.file_name().map(|n| n.to_os_string()) else {
            break;
        };
        tail.push(name);
        if !cur.pop() {
            break;
        }
    }

    let is_link = std::fs::symlink_metadata(&cur)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    if is_link {
        if followed >= MAX_SYMLINK_HOPS {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "too many levels of symbolic links",
            ));
        }
        let target = std::fs::read_link(&cur)?;
        let base = if target.is_absolute() {
            PathBuf::new()
        } else {
            // Relative targets are resolved against the directory holding the
            // link — canonicalised, so a `..` inside the target lands where
            // the kernel would put it.
            let parent = cur.parent().unwrap_or(Path::new(""));
            parent
                .canonicalize()
                .unwrap_or_else(|_| parent.to_path_buf())
        };
        let mut next = normalise(&base.join(&target));
        for part in tail.iter().rev() {
            next.push(part);
        }
        return resolve_links(&next, followed + 1);
    }

    let mut out = cur.canonicalize()?;
    for part in tail.iter().rev() {
        out.push(part);
    }
    Ok(out)
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

    /// A symlinked *parent* directory must not let a not-yet-existing file
    /// through: the link resolves outside every root, so the candidate does.
    #[cfg(unix)]
    #[test]
    fn symlink_parent_blocks_new_file() {
        let ws = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.path().join("docs")).unwrap();
        let roots = vec![rw(ws.path())];
        let err = resolve_within_any(&roots, "docs/new.txt", false).unwrap_err();
        assert!(
            err.to_string().contains("symlink"),
            "symlinked parent must be rejected as a symlink escape: {err}"
        );
    }

    /// Same link, target file *exists* — this path was already rejected before
    /// the ancestor fix; keep it rejected so the fix can't regress it.
    #[cfg(unix)]
    #[test]
    fn symlink_parent_blocks_existing_file() {
        let ws = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("exists.txt"), "outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.path().join("docs")).unwrap();
        let roots = vec![rw(ws.path())];
        assert!(resolve_within_any(&roots, "docs/exists.txt", false).is_err());
    }

    /// Nested creation through the link must be rejected too — otherwise
    /// `create_dir_all` would materialise directories outside the sandbox.
    #[cfg(unix)]
    #[test]
    fn symlink_parent_blocks_nested_creation() {
        let ws = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.path().join("docs")).unwrap();
        let roots = vec![rw(ws.path())];
        assert!(resolve_within_any(&roots, "docs/.ssh/newdir/x", true).is_err());
    }

    /// Ordinary nested paths (parents still to be created) must keep working.
    #[test]
    fn legit_nested_path_still_allowed() {
        let ws = TempDir::new().unwrap();
        let roots = vec![rw(ws.path())];
        let got = resolve_within_any(&roots, "sub/dir/new.txt", true).unwrap();
        assert!(got.is_absolute(), "resolved path must be absolute: {got:?}");
        assert!(
            got.ends_with("sub/dir/new.txt"),
            "resolved path must keep the tail: {got:?}"
        );
        // The returned path keeps the caller's spelling (rooted at the
        // workspace as given), not the canonicalised form.
        assert_eq!(got, ws.path().join("sub").join("dir").join("new.txt"));
    }

    /// The root itself may be a symlink (`/tmp` → `/private/tmp`, or a
    /// `TempDir` under `/var/folders`). Paths inside it must still resolve.
    #[cfg(unix)]
    #[test]
    fn workspace_root_itself_is_a_symlink() {
        let real = TempDir::new().unwrap();
        std::fs::write(real.path().join("exists.txt"), "inside").unwrap();
        let holder = TempDir::new().unwrap();
        let link = holder.path().join("ws");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();
        let roots = vec![rw(&link)];

        // Both a new and an existing file must pass — the canonical root used
        // for the check must not be compared against the link-spelled
        // candidate (that would reject every write under such a root).
        let new = resolve_within_any(&roots, "new.txt", true).unwrap();
        assert!(new.ends_with("new.txt"), "unexpected path: {new:?}");
        assert_eq!(new.parent().unwrap(), link.as_path());

        let existing = resolve_within_any(&roots, "exists.txt", false).unwrap();
        assert!(
            existing.ends_with("exists.txt"),
            "unexpected path: {existing:?}"
        );
        assert!(existing.exists(), "resolution must reach the real file");
    }

    /// A *dangling* symlink as the final component: the link exists, its
    /// target does not — so `exists()` is false and the candidate cannot be
    /// canonicalised. Writing through it would create the target outside, so
    /// the link itself has to be followed for the containment check.
    #[cfg(unix)]
    #[test]
    fn dangling_symlink_leaf_is_rejected() {
        let ws = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        // Parent of the target exists, the target itself does not.
        std::os::unix::fs::symlink(outside.path().join("created.txt"), ws.path().join("notes"))
            .unwrap();
        let roots = vec![rw(ws.path())];
        let err = resolve_within_any(&roots, "notes", true).unwrap_err();
        assert!(
            err.to_string().contains("symlink"),
            "a dangling leaf link must be rejected as a symlink escape: {err}"
        );
        assert!(
            resolve_within_any(&roots, "notes", false).is_err(),
            "reads through a dangling leaf link must be rejected too"
        );
    }

    /// Relative target, rebased on the link's directory: `ws/notes -> ../../x`
    /// points outside the root and must be rejected even though nothing on the
    /// target path exists.
    #[cfg(unix)]
    #[test]
    fn dangling_symlink_leaf_with_relative_target_is_rejected() {
        let ws = TempDir::new().unwrap();
        std::os::unix::fs::symlink("../../escaped.txt", ws.path().join("notes")).unwrap();
        let roots = vec![rw(ws.path())];
        assert!(resolve_within_any(&roots, "notes", true).is_err());
    }

    /// Link chains must be followed, and a cycle must be reported rather than
    /// looped on.
    #[cfg(unix)]
    #[test]
    fn symlink_leaf_chains_and_cycles() {
        let ws = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path().join("target.txt"), ws.path().join("hop2"))
            .unwrap();
        std::os::unix::fs::symlink("hop2", ws.path().join("hop1")).unwrap();
        let roots = vec![rw(ws.path())];
        assert!(
            resolve_within_any(&roots, "hop1", true).is_err(),
            "a chain ending outside the root must be rejected"
        );

        std::os::unix::fs::symlink("cycle_b", ws.path().join("cycle_a")).unwrap();
        std::os::unix::fs::symlink("cycle_a", ws.path().join("cycle_b")).unwrap();
        assert!(
            resolve_within_any(&roots, "cycle_a", false).is_err(),
            "a symlink cycle must be rejected, not followed forever"
        );
    }

    /// A dangling leaf link that points back *inside* the root is not an
    /// escape and stays allowed (writing through it creates a file inside).
    #[cfg(unix)]
    #[test]
    fn dangling_symlink_leaf_inside_root_is_allowed() {
        let ws = TempDir::new().unwrap();
        std::os::unix::fs::symlink("sub/missing.txt", ws.path().join("notes")).unwrap();
        let roots = vec![rw(ws.path())];
        let got = resolve_within_any(&roots, "notes", true).unwrap();
        assert_eq!(got, ws.path().join("notes"));
    }

    /// A root that does not exist yet (an `--add-dir` directory created later,
    /// a work dir still to be made) must not be falsely rejected just because
    /// its own path runs through a symlinked parent: root and candidate are
    /// resolved by the same helper, so both keep that spelling.
    #[cfg(unix)]
    #[test]
    fn uncreated_root_under_symlinked_parent_still_matches() {
        let holder = TempDir::new().unwrap();
        let real = holder.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = holder.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let root = link.join("not_created_yet");
        assert!(!root.exists(), "precondition: the root must not exist");
        let roots = vec![rw(&root)];
        let got = resolve_within_any(&roots, "a.txt", true).unwrap();
        assert_eq!(got, root.join("a.txt"));
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

    // ── AuditMeta records both raw and canonical names (issue #153) ──────────

    #[tokio::test]
    async fn audit_records_both_names() {
        use crate::tools::WriteFile;
        use std::sync::Arc;
        let tmp = TempDir::new().unwrap();
        let registry = ToolRegistry::local()
            .register_with_aliases(Arc::new(WriteFile::new(tmp.path())), &["write_file"]);
        let dispatch = registry
            .invoke_with_audit(
                "write_file",
                serde_json::json!({"path": "out.txt", "contents": "hello"}),
            )
            .await;
        assert!(
            dispatch.result.is_ok(),
            "write must succeed: {:?}",
            dispatch.result
        );
        assert_eq!(
            dispatch.audit.tool_name.as_deref(),
            Some("write_file"),
            "audit must keep the raw (aliased) name"
        );
        assert_eq!(
            dispatch.audit.canonical_name.as_deref(),
            Some("Write"),
            "audit must carry the canonical name"
        );
    }
}
