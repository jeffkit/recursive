//! Tool dispatch: the execution path from `invoke` to tool result.
//!
//! Contains [`ToolDispatch`] (the return type of `invoke_with_audit`),
//! the dispatch-stage methods of [`ToolRegistry`], touched-file recording,
//! argument preview generation, and path containment helpers.

use serde_json::Value;
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
pub fn resolve_within(root: &std::path::Path, path: &str) -> Result<std::path::PathBuf> {
    let candidate = std::path::Path::new(path);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    let abs_root = absolutise(root);
    let abs_joined = absolutise(&joined);
    // Lexical check (works for paths that don't exist yet).
    if !abs_joined.starts_with(&abs_root) {
        return Err(Error::BadToolArgs {
            name: "<fs>".into(),
            message: format!(
                "path `{}` escapes workspace root `{}`",
                path,
                abs_root.display()
            ),
        });
    }
    // Symlink-aware check: if the path exists, canonicalize both sides and
    // re-check so that symlinks pointing outside the workspace are rejected.
    if abs_joined.exists() {
        let canonical_root = abs_root.canonicalize().map_err(|e| Error::BadToolArgs {
            name: "<fs>".into(),
            message: format!(
                "cannot canonicalize workspace root `{}`: {}",
                abs_root.display(),
                e
            ),
        })?;
        match abs_joined.canonicalize() {
            Ok(canonical_joined) => {
                if !canonical_joined.starts_with(&canonical_root) {
                    return Err(Error::BadToolArgs {
                        name: "<fs>".into(),
                        message: format!(
                            "path `{}` resolves via symlink to a location outside the workspace root `{}`",
                            path,
                            canonical_root.display()
                        ),
                    });
                }
            }
            Err(e) => {
                return Err(Error::BadToolArgs {
                    name: "<fs>".into(),
                    message: format!("cannot resolve path `{}`: {e}", path),
                });
            }
        }
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
