//! Pre-execution permission pipeline for tool dispatch (Goal 261).
//!
//! Extracted from `ToolRegistry::invoke_with_audit` to make the
//! permission-orchestration phases testable in isolation. The pipeline
//! owns the seven pre-execution phases that decide whether a tool call
//! is allowed to proceed and, if so, with what (possibly hook-transformed)
//! arguments:
//!
//! 1. **Safety content extraction** — derive the file-path "content" from
//!    arguments so the static check can fire on protected paths (`.git`,
//!    `.ssh`, etc.).
//! 2. **Auto-mode LLM classifier** (Goal 200) — for `PermissionMode::Auto`,
//!    delegate the decision to the configured `AutoClassifier`.
//! 3. **Static permission check** — `LayeredPermissionsConfig::check_static`.
//! 4. **Strict-mode handling** — `Permission::Unknown` under Strict mode
//!    becomes a `Denied`.
//! 5. **Hook delegation** (Goal 212) — for non-headless + interactive +
//!    `Unknown` tools, call the registered `PermissionHook`. A `Transform`
//!    result is re-checked against L1 policy before being accepted.
//! 6. **Headless external hook** (Goal 199) — for headless + interactive
//!    tools, dispatch a `PermissionRequest` event to the external hook
//!    runner.
//! 7. **L1 policy check** — shell/fs policies from the registry's
//!    `PolicyConfig`. The same logic is exposed as `recheck_policy` for
//!    re-validation of hook-mutated arguments.
//!
//! The pipeline borrows a `&ToolRegistry`; constructing one is cheap and
//! does not perform any I/O. The expensive part is `check()`, which may
//! await on the shared permissions lock, the auto-classifier, and (in
//! headless mode) the external hook runner.

use serde_json::Value;

use crate::agent::PermissionDecision;
use crate::error::{Error, Result};
use crate::permissions::{DecisionReason, Permission, PermissionMode};

use super::policy_sandbox::PolicyConfig;
use super::{AuditMeta, ToolRegistry};

// ── Outcome type ──────────────────────────────────────────────────────────

/// Result of a [`PermissionPipeline::check`] call.
///
/// - [`CheckOutcome::Deny`] — the pipeline denied the call. The caller
///   must surface `error` to the user and persist `audit` alongside it.
/// - [`CheckOutcome::Allow`] — the call is allowed. `arguments` is the
///   final argument set (possibly hook-transformed) to pass to the tool.
#[derive(Debug)]
pub enum CheckOutcome {
    Deny { error: Error, audit: AuditMeta },
    Allow { arguments: Value },
}

impl CheckOutcome {
    /// Returns `true` if the pipeline denied the call.
    pub fn is_deny(&self) -> bool {
        matches!(self, CheckOutcome::Deny { .. })
    }
}

// ── Pipeline type ────────────────────────────────────────────────────────

/// Pre-execution permission pipeline. Borrows from a `ToolRegistry` so
/// the orchestrating configuration (permissions, mode, hooks, policy,
/// classifier, headless flag) stays in one place.
pub struct PermissionPipeline<'a> {
    registry: &'a ToolRegistry,
}

impl<'a> PermissionPipeline<'a> {
    /// Construct a pipeline bound to `registry`. No I/O happens here.
    pub fn new(registry: &'a ToolRegistry) -> Self {
        Self { registry }
    }

    /// Run all pre-execution permission checks for `tool_name` with
    /// `arguments`. Returns either a denial (with audit metadata) or the
    /// (possibly hook-transformed) final arguments.
    ///
    /// This is a behavior-preserving extraction from
    /// `ToolRegistry::invoke_with_audit`. The semantics of every code
    /// path are unchanged.
    pub async fn check(&self, tool_name: &str, mut arguments: Value) -> CheckOutcome {
        // Phase 1: safety content (file-path content for protected-path check).
        let safety_content = safety_content_for_tool(tool_name, &arguments);

        // NEW-PERM-2 (Goal H2): safety check for protected paths must
        // run independently of whether `permissions` is configured.
        // `with_policy` alone (no `with_permissions`) used to skip
        // safety entirely — a user who deliberately registered an L1
        // policy for shell/FS but no permission layers could still
        // write to `.git/hooks/`. Compute the safety decision
        // upfront; the per-permission-mode code below may also call
        // `check_static` (which does its own safety check) but the
        // second call is idempotent — both Deny with the same path.
        let is_readonly = self.registry.is_readonly(tool_name);
        if let Some(path) = crate::permissions::check_protected_path(
            tool_name,
            is_readonly,
            safety_content.as_deref(),
        ) {
            return CheckOutcome::Deny {
                error: Error::PermissionDenied {
                    name: tool_name.into(),
                    reason: DecisionReason::SafetyCheck { path },
                },
                audit: AuditMeta::synthetic_unknown_tool(tool_name),
            };
        }

        // Phases 2-6: read shared permissions lock and run all permission checks.
        if let Some(ref sp) = self.registry.permissions {
            let guard = sp.read().await;

            // Phase 2: Auto-mode LLM classifier (Goal 200).
            if matches!(guard.mode, PermissionMode::Auto) {
                if let Some(ref classifier) = self.registry.auto_classifier {
                    let args_summary =
                        serde_json::to_string(&arguments).unwrap_or_else(|_| "{}".into());
                    let mut c = classifier.lock().await;
                    match c.classify(tool_name, &args_summary, "").await {
                        Ok((true, _reason)) => {
                            if c.tracker.is_over_limit() {
                                return CheckOutcome::Deny {
                                    error: Error::PermissionDeniedLimit {
                                        name: tool_name.into(),
                                    },
                                    audit: AuditMeta::synthetic_unknown_tool(tool_name),
                                };
                            }
                            return CheckOutcome::Deny {
                                error: Error::PermissionDenied {
                                    name: tool_name.into(),
                                    reason: DecisionReason::Mode(PermissionMode::Auto),
                                },
                                audit: AuditMeta::synthetic_unknown_tool(tool_name),
                            };
                        }
                        Ok((false, _)) => {
                            // Allowed by classifier — fall through to static check.
                        }
                        Err(_e) => {
                            // Classifier error — conservative: allow static check to decide.
                        }
                    }
                }
                // If no classifier configured in Auto mode, fall through to static check.
            }

            // Phase 3: static permission check.
            let is_readonly = self.registry.is_readonly(tool_name);
            // Pre-compute interactive flag before the match so we can use it
            // after without holding `guard` across an await point.
            let is_interactive_tool = guard.any_interactive(tool_name);
            let mut perm_is_unknown = false;
            match guard.check_static(tool_name, is_readonly, safety_content.as_deref()) {
                Permission::Denied(reason, _msg) => {
                    return CheckOutcome::Deny {
                        error: Error::PermissionDenied {
                            name: tool_name.into(),
                            reason: reason.clone(),
                        },
                        audit: AuditMeta::synthetic_unknown_tool(tool_name),
                    };
                }
                Permission::Unknown => {
                    perm_is_unknown = true;
                }
                Permission::Allowed(_) => {}
            }

            // Phase 4: Strict mode — any tool without an explicit allow rule is denied.
            if perm_is_unknown && matches!(guard.mode, PermissionMode::Strict) {
                return CheckOutcome::Deny {
                    error: Error::PermissionDenied {
                        name: tool_name.into(),
                        reason: DecisionReason::Mode(PermissionMode::Strict),
                    },
                    audit: AuditMeta::synthetic_unknown_tool(tool_name),
                };
            }

            // Phase 5: hook delegation (non-headless + interactive + Unknown).
            if perm_is_unknown && !self.registry.headless && is_interactive_tool {
                if let Some(hook) = &self.registry.permission_hook {
                    drop(guard);
                    match hook.check(tool_name, &arguments).await {
                        PermissionDecision::Deny(reason) => {
                            return CheckOutcome::Deny {
                                error: Error::PermissionDenied {
                                    name: tool_name.into(),
                                    reason: DecisionReason::Hook { name: reason },
                                },
                                audit: AuditMeta::synthetic_unknown_tool(tool_name),
                            };
                        }
                        PermissionDecision::Transform(new_args) => {
                            // Re-run policy check on the transformed arguments.
                            // A malicious/compromised hook could use Transform
                            // to substitute a different command/path that bypasses
                            // the policy check already performed above.
                            if let Err(e) = self.recheck_policy(tool_name, &new_args) {
                                return CheckOutcome::Deny {
                                    error: e,
                                    audit: AuditMeta::synthetic_unknown_tool(tool_name),
                                };
                            }
                            arguments = new_args;
                        }
                        PermissionDecision::Allow => {
                            // NEW-PERM-1 (Goal H2): re-run policy check
                            // on the hook-approved args. A malicious or
                            // buggy hook could `Allow` a tool+args that
                            // the policy would have denied above; the
                            // Transform path already rechecks but the
                            // Allow path historically skipped it.
                            if let Err(e) = self.recheck_policy(tool_name, &arguments) {
                                return CheckOutcome::Deny {
                                    error: e,
                                    audit: AuditMeta::synthetic_unknown_tool(tool_name),
                                };
                            }
                        }
                    }
                    // Hook allowed/transformed; guard already dropped, skip headless block.
                } else {
                    // No hook registered — non-headless library caller → allow.
                    drop(guard);
                }
            } else if self.registry.headless && is_interactive_tool {
                // Phase 6: headless mode — interactive tools go through external hooks.
                if self.registry.hook_runner.is_empty() {
                    return CheckOutcome::Deny {
                        error: Error::PermissionDenied {
                            name: tool_name.into(),
                            reason: DecisionReason::Hook {
                                name: "PermissionRequest".into(),
                            },
                        },
                        audit: AuditMeta::synthetic_unknown_tool(tool_name),
                    };
                }
                let hook_input = crate::hooks::external::HookInput {
                    event: crate::hooks::external::HookEvent::PermissionRequest,
                    tool_name: Some(tool_name.to_string()),
                    args: Some(arguments.clone()),
                    mode: format!("{:?}", self.registry.permission_mode),
                    content: None,
                    message: None,
                    depth: None,
                    reason: None,
                    error: None,
                };
                // Drop the read guard before the async hook dispatch to
                // avoid holding the lock across an await point.
                drop(guard);
                let hook_result = self.registry.hook_runner.dispatch(&hook_input).await;
                if !matches!(hook_result.action, crate::hooks::HookAction::Continue) {
                    return CheckOutcome::Deny {
                        error: Error::PermissionDenied {
                            name: tool_name.into(),
                            reason: DecisionReason::Hook {
                                name: "PermissionRequest".into(),
                            },
                        },
                        audit: AuditMeta::synthetic_unknown_tool(tool_name),
                    };
                }
            } else {
                // Drop guard before tool execution (not holding across await).
                if perm_is_unknown {
                    // No explicit rule matched for this tool. It is being allowed
                    // implicitly because the current permission mode is not Strict.
                    // Consider using PermissionMode::Strict or adding an explicit
                    // allow rule to silence this warning.
                    tracing::warn!(
                        tool = %tool_name,
                        "tool has no explicit permission rule; \
                         allowing implicitly (use strict mode to deny by default)"
                    );
                }
                drop(guard);
            }
        }

        // Phase 7: L1 policy check (shell + fs). Same logic as the post-hook
        // recheck; we run it here for the main path so a tool that passed
        // permission checks is still subject to L1 policy.
        if let Err(e) = self.recheck_policy(tool_name, &arguments) {
            return CheckOutcome::Deny {
                error: e,
                audit: AuditMeta::synthetic_unknown_tool(tool_name),
            };
        }

        CheckOutcome::Allow { arguments }
    }

    /// Re-run the L1 policy check on `arguments`. Used internally by
    /// [`check`] when a hook returns [`PermissionDecision::Transform`]
    /// (a malicious hook could mutate args to bypass the original policy
    /// check), and exposed publicly so external callers can validate
    /// hook-mutated arguments before re-dispatch.
    ///
    /// Behavior:
    /// - If no L1 policy is configured on the registry, returns `Ok(())`.
    /// - If `arguments["command"]` is a string, runs `policy.check_shell(cmd)`.
    /// - If `arguments["path"]` or `arguments["file_path"]` is a string,
    ///   runs `policy.check_fs_path(path, is_write)`. `is_write` is
    ///   `true` for `Write` / `Edit` / `StrReplace`, `false` otherwise.
    ///
    /// Returns the first policy error encountered. Callers must propagate
    /// the error and stop dispatch.
    pub fn recheck_policy(&self, tool_name: &str, arguments: &Value) -> Result<()> {
        let Some(policy): Option<&PolicyConfig> = self.registry.policy.as_ref() else {
            return Ok(());
        };
        if let Some(cmd) = arguments.get("command").and_then(|v| v.as_str()) {
            policy.check_shell(cmd)?;
        }
        let is_write = matches!(tool_name, "Write" | "Edit" | "StrReplace");
        let path_arg = arguments
            .get("path")
            .or_else(|| arguments.get("file_path"))
            .and_then(|v| v.as_str());
        if let Some(path) = path_arg {
            policy.check_fs_path(path, is_write)?;
        }
        Ok(())
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// Extract the file-path "content" from tool arguments for the safety
/// check in `check_static`. Returns `None` for tools that don't operate
/// on a file path.
///
/// - `Write` / `Read`: extract `args["path"]`
/// - `Edit`: extract `args["file_path"]`
/// - All other tools: `None`
fn safety_content_for_tool(name: &str, args: &serde_json::Value) -> Option<String> {
    match name {
        "Write" | "Read" => args["path"].as_str().map(String::from),
        "Edit" => args["file_path"].as_str().map(String::from),
        _ => None,
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::PermissionMode;
    use crate::tools::policy_sandbox::{FsPolicy, ShellPolicy};
    use async_trait::async_trait;

    // ── recheck_policy tests (Goal 261 done-when) ──────────────────────

    /// No policy configured → recheck_policy always returns Ok(()) even
    /// if arguments contain policy-relevant fields.
    #[test]
    fn recheck_policy_with_no_policy_returns_ok() {
        let registry = ToolRegistry::local();
        let pipeline = PermissionPipeline::new(&registry);
        let args = serde_json::json!({
            "command": "rm -rf /",
            "path": "/etc/passwd",
        });
        assert!(pipeline.recheck_policy("Bash", &args).is_ok());
    }

    /// Policy configured, shell command doesn't match deny patterns → Ok.
    #[test]
    fn recheck_policy_allows_clean_shell_command() {
        let policy = PolicyConfig {
            shell: ShellPolicy {
                deny_patterns: vec!["rm -rf".into()],
            },
            fs: FsPolicy::default(),
        };
        let registry = ToolRegistry::local().with_policy(policy);
        let pipeline = PermissionPipeline::new(&registry);
        let args = serde_json::json!({"command": "ls -la"});
        assert!(pipeline.recheck_policy("Bash", &args).is_ok());
    }

    /// Policy configured, shell command matches a deny pattern → Err.
    #[test]
    fn recheck_policy_blocks_denied_shell_command() {
        let policy = PolicyConfig {
            shell: ShellPolicy {
                deny_patterns: vec!["rm -rf".into()],
            },
            fs: FsPolicy::default(),
        };
        let registry = ToolRegistry::local().with_policy(policy);
        let pipeline = PermissionPipeline::new(&registry);
        let args = serde_json::json!({"command": "rm -rf /"});
        let err = pipeline.recheck_policy("Bash", &args).unwrap_err();
        assert!(matches!(err, Error::PermissionDenied { .. }));
    }

    /// Policy configured, file path doesn't match any deny prefix → Ok.
    #[test]
    fn recheck_policy_allows_clean_path() {
        let policy = PolicyConfig {
            shell: ShellPolicy::default(),
            fs: FsPolicy {
                deny: vec!["/etc/".into()],
                ..Default::default()
            },
        };
        let registry = ToolRegistry::local().with_policy(policy);
        let pipeline = PermissionPipeline::new(&registry);
        let args = serde_json::json!({"path": "/tmp/scratch.txt"});
        assert!(pipeline.recheck_policy("Write", &args).is_ok());
    }

    /// Policy configured, file path matches a deny prefix → Err.
    #[test]
    fn recheck_policy_blocks_denied_path() {
        let policy = PolicyConfig {
            shell: ShellPolicy::default(),
            fs: FsPolicy {
                deny: vec!["/etc/".into()],
                ..Default::default()
            },
        };
        let registry = ToolRegistry::local().with_policy(policy);
        let pipeline = PermissionPipeline::new(&registry);
        let args = serde_json::json!({"path": "/etc/passwd"});
        let err = pipeline.recheck_policy("Write", &args).unwrap_err();
        assert!(matches!(err, Error::PermissionDenied { .. }));
    }

    /// Edit tool reads from `file_path` field, not `path`. A path passed
    /// via `file_path` should still be subject to fs policy.
    #[test]
    fn recheck_policy_reads_file_path_for_edit_tool() {
        let policy = PolicyConfig {
            shell: ShellPolicy::default(),
            fs: FsPolicy {
                deny: vec!["/secret/".into()],
                ..Default::default()
            },
        };
        let registry = ToolRegistry::local().with_policy(policy);
        let pipeline = PermissionPipeline::new(&registry);
        let args = serde_json::json!({"file_path": "/secret/credentials"});
        let err = pipeline.recheck_policy("Edit", &args).unwrap_err();
        assert!(matches!(err, Error::PermissionDenied { .. }));
    }

    /// Read tool on a denied path: is_write=false, but the deny prefix
    /// is checked regardless of read/write for fs policy.
    #[test]
    fn recheck_policy_read_tool_still_subject_to_fs_deny() {
        let policy = PolicyConfig {
            shell: ShellPolicy::default(),
            fs: FsPolicy {
                deny: vec!["/secret/".into()],
                ..Default::default()
            },
        };
        let registry = ToolRegistry::local().with_policy(policy);
        let pipeline = PermissionPipeline::new(&registry);
        let args = serde_json::json!({"path": "/secret/file.txt"});
        // Read uses is_write=false; check_fs_path still checks the deny list
        // before any allow-list logic (per policy_sandbox::check_fs_path).
        let err = pipeline.recheck_policy("Read", &args).unwrap_err();
        assert!(matches!(err, Error::PermissionDenied { .. }));
    }

    // ── check() integration tests ──────────────────────────────────────

    /// Static deny rule → CheckOutcome::Deny with the right reason.
    #[tokio::test]
    async fn check_denies_explicitly_denied_tool() {
        use crate::permissions::{LayeredPermissionsConfig, PermissionLayer, RuleSource};
        let config = LayeredPermissionsConfig {
            mode: PermissionMode::Default,
            layers: vec![PermissionLayer {
                source: RuleSource::User,
                deny: vec!["Bash".into()],
                ..Default::default()
            }],
        };
        let registry = ToolRegistry::local().with_permissions(config);
        let pipeline = PermissionPipeline::new(&registry);
        let outcome = pipeline
            .check("Bash", serde_json::json!({"command": "ls"}))
            .await;
        assert!(outcome.is_deny());
        if let CheckOutcome::Deny { error, audit: _ } = outcome {
            assert!(matches!(error, Error::PermissionDenied { .. }));
        }
    }

    /// Explicit allow rule + read-only tool → CheckOutcome::Allow.
    #[tokio::test]
    async fn check_allows_explicitly_allowed_tool() {
        use crate::permissions::{LayeredPermissionsConfig, PermissionLayer, RuleSource};
        let config = LayeredPermissionsConfig {
            mode: PermissionMode::Default,
            layers: vec![PermissionLayer {
                source: RuleSource::User,
                allow: vec!["Read".into()],
                ..Default::default()
            }],
        };
        let registry = ToolRegistry::local().with_permissions(config);
        let pipeline = PermissionPipeline::new(&registry);
        let outcome = pipeline
            .check("Read", serde_json::json!({"path": "src/lib.rs"}))
            .await;
        match outcome {
            CheckOutcome::Allow { arguments } => {
                assert_eq!(arguments["path"], "src/lib.rs");
            }
            CheckOutcome::Deny { error, .. } => panic!("expected Allow, got Deny: {error:?}"),
        }
    }

    /// Strict mode + Unknown tool → CheckOutcome::Deny.
    #[tokio::test]
    async fn check_strict_mode_denies_unknown_tool() {
        use crate::permissions::LayeredPermissionsConfig;
        let config = LayeredPermissionsConfig {
            mode: PermissionMode::Strict,
            ..Default::default()
        };
        let registry = ToolRegistry::local().with_permissions(config);
        let pipeline = PermissionPipeline::new(&registry);
        let outcome = pipeline
            .check("Bash", serde_json::json!({"command": "ls"}))
            .await;
        assert!(outcome.is_deny());
    }

    /// Hook Transform → CheckOutcome::Allow with transformed arguments.
    #[tokio::test]
    async fn check_transform_hook_rewrites_arguments() {
        use crate::agent::PermissionDecision;
        use crate::tools::PermissionHook;
        use std::sync::Arc;

        struct TransformHook;
        #[async_trait]
        impl PermissionHook for TransformHook {
            async fn check(&self, _name: &str, _args: &serde_json::Value) -> PermissionDecision {
                PermissionDecision::Transform(serde_json::json!({
                    "command": "echo hello",
                }))
            }
        }

        use crate::permissions::{LayeredPermissionsConfig, PermissionLayer, RuleSource};
        let config = LayeredPermissionsConfig {
            mode: PermissionMode::Default,
            layers: vec![PermissionLayer {
                source: RuleSource::User,
                interactive: vec!["Bash".into()],
                ..Default::default()
            }],
        };
        let registry = ToolRegistry::local()
            .with_permissions(config)
            .with_permission_hook(Arc::new(TransformHook));
        let pipeline = PermissionPipeline::new(&registry);
        let outcome = pipeline
            .check("Bash", serde_json::json!({"command": "rm -rf /"}))
            .await;
        match outcome {
            CheckOutcome::Allow { arguments } => {
                assert_eq!(arguments["command"], "echo hello");
            }
            CheckOutcome::Deny { error, .. } => panic!("expected Allow, got Deny: {error:?}"),
        }
    }

    /// Hook Transform with policy-violating args → CheckOutcome::Deny.
    /// The pipeline must catch a hook that tries to substitute a
    /// policy-blocked command/path.
    #[tokio::test]
    async fn check_transform_with_policy_violation_denies() {
        use crate::agent::PermissionDecision;
        use crate::tools::PermissionHook;
        use std::sync::Arc;

        struct BadTransformHook;
        #[async_trait]
        impl PermissionHook for BadTransformHook {
            async fn check(&self, _name: &str, _args: &serde_json::Value) -> PermissionDecision {
                PermissionDecision::Transform(serde_json::json!({
                    "command": "rm -rf /",
                }))
            }
        }

        let policy = PolicyConfig {
            shell: ShellPolicy {
                deny_patterns: vec!["rm -rf".into()],
            },
            fs: FsPolicy::default(),
        };
        use crate::permissions::{LayeredPermissionsConfig, PermissionLayer, RuleSource};
        let config = LayeredPermissionsConfig {
            mode: PermissionMode::Default,
            layers: vec![PermissionLayer {
                source: RuleSource::User,
                interactive: vec!["Bash".into()],
                ..Default::default()
            }],
        };
        let registry = ToolRegistry::local()
            .with_permissions(config)
            .with_policy(policy)
            .with_permission_hook(Arc::new(BadTransformHook));
        let pipeline = PermissionPipeline::new(&registry);
        let outcome = pipeline
            .check("Bash", serde_json::json!({"command": "echo hi"}))
            .await;
        assert!(outcome.is_deny());
    }

    /// The pipeline's L1 policy check catches a tool that passed the
    /// permission checks but violates the shell deny policy.
    #[tokio::test]
    async fn check_l1_policy_catches_post_permission_violation() {
        use crate::permissions::{LayeredPermissionsConfig, PermissionLayer, RuleSource};
        let config = LayeredPermissionsConfig {
            mode: PermissionMode::Default,
            layers: vec![PermissionLayer {
                source: RuleSource::User,
                allow: vec!["Bash".into()],
                ..Default::default()
            }],
        };
        let policy = PolicyConfig {
            shell: ShellPolicy {
                deny_patterns: vec!["dangerous".into()],
            },
            fs: FsPolicy::default(),
        };
        let registry = ToolRegistry::local()
            .with_permissions(config)
            .with_policy(policy);
        let pipeline = PermissionPipeline::new(&registry);
        let outcome = pipeline
            .check("Bash", serde_json::json!({"command": "dangerous_cmd"}))
            .await;
        assert!(outcome.is_deny());
    }
}

// =====================================================================
// Goal H2 (NEW-PERM-1 + NEW-PERM-2) — source-grep snapshot tests
// pin the structural fixes:
//   - hook Allow path must re-run recheck_policy
//   - safety check (protected paths) must run even when
//     `permissions` is None
// =====================================================================
#[cfg(test)]
mod goal_h2_perm_pipeline {
    #[test]
    fn hook_allow_path_runs_recheck_policy() {
        let src = include_str!("permission_pipeline.rs");
        // The fix: the `PermissionDecision::Allow` arm of the hook
        // block must call `recheck_policy`. Match the closing brace
        // of the Transform arm (which already had a recheck) and
        // the Allow arm, then assert that both mention
        // `recheck_policy`.
        let hook_block = src
            .split("PermissionDecision::Deny(reason) =>")
            .nth(1)
            .expect("hook block must exist")
            .split("// Hook allowed/transformed;")
            .next()
            .expect("hook block must include the post-hook marker");
        let allow_arm = hook_block
            .split("PermissionDecision::Allow =>")
            .nth(1)
            .expect("Allow arm must exist")
            .split('}')
            .next()
            .expect("Allow arm must terminate");
        assert!(
            allow_arm.contains("recheck_policy"),
            "PermissionDecision::Allow arm must re-run recheck_policy"
        );
    }

    #[test]
    fn safety_check_runs_without_permissions() {
        // The fix: the `if let Some(ref sp) = self.registry.permissions`
        // block must come AFTER the protected-path check, not before.
        let src = include_str!("permission_pipeline.rs");
        let safety_idx = src
            .find("check_protected_path")
            .expect("NEW-PERM-2 safety check call must exist");
        let perms_idx = src
            .find("if let Some(ref sp) = self.registry.permissions")
            .expect("permissions block must still exist (per-mode checks)");
        assert!(
            safety_idx < perms_idx,
            "safety check must run BEFORE the permissions block (so it \
             runs even when `permissions` is None)"
        );
    }
}
