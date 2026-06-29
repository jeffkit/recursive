//! Tool abstraction: any side effect the model can request.
//!
//! Tools are orthogonal to the agent and to each other. To add a capability
//! you implement `Tool` and register it; no other file changes.

// ── Sub-modules ────────────────────────────────────────────────────────────

pub mod a2a;
pub mod agent;
pub mod agent_defs;
pub mod audit;
pub mod checkpoint;
pub mod count_lines;
pub mod dispatch;
#[cfg(feature = "cloud-runtime")]
pub mod docker_provider;
#[cfg(feature = "cloud-runtime")]
pub mod docker_sandbox;
#[cfg(feature = "e2b-sandbox")]
pub mod e2b_provider;
pub mod edit;
pub mod episodic_recall;
pub mod estimate_tokens;
pub mod facts;
#[cfg(feature = "skill-hub")]
pub mod find_skills;
pub mod fs;
pub mod glob;
#[cfg(feature = "skill-hub")]
pub mod install_skill;
pub mod load_skill;
pub mod memory;
pub mod permission_pipeline;
pub mod plan_mode;
pub mod policy;
pub mod policy_sandbox;
pub mod registry;
pub mod run_background;
pub mod run_skill_script;
pub mod schedule_wakeup;
pub mod search;
pub mod send_message;
pub mod shell;

#[cfg(feature = "coordinator-mode")]
pub mod task_create;
#[cfg(feature = "coordinator-mode")]
pub mod task_get;
#[cfg(feature = "coordinator-mode")]
pub mod task_list;
#[cfg(feature = "coordinator-mode")]
pub mod task_output;
#[cfg(feature = "coordinator-mode")]
pub mod task_stop;
#[cfg(feature = "coordinator-mode")]
pub mod task_update;
#[cfg(feature = "coordinator-mode")]
pub mod team_create;
#[cfg(feature = "coordinator-mode")]
pub mod team_delete;
pub mod todo;
pub mod tool_search;
pub mod transport;
#[cfg(feature = "web_fetch")]
pub mod web_fetch;
#[cfg(feature = "web_search")]
pub mod web_search;

// ── Re-exports from registry ────────────────────────────────────────────────

pub use registry::{
    build_standard_tools, build_standard_tools_with_roots, PermissionHook, SpecWithHint, Tool,
    ToolRegistry,
};

// ── Re-exports from audit ───────────────────────────────────────────────────

pub use audit::{
    AuditKey, AuditMeta, ExitStatus, ToolSideEffect, TouchedFiles, AUDIT_ERR_MAX_BYTES,
};

// ── Re-exports from dispatch ────────────────────────────────────────────────

pub use dispatch::resolve_within;
pub use dispatch::{args_preview_for_permission, ToolDispatch};
pub use dispatch::{new_shared_sandbox_roots, resolve_within_any, AccessTier, SharedSandboxRoots};

// ── Re-exports from individual tool modules ─────────────────────────────────

pub use a2a::{A2aCallTool, A2aCardTool, A2aTaskCheckTool};
pub use agent::{AgentTool, SharedMemoryRead, SharedMemoryWrite};
pub use agent_defs::{AgentDefinition, AgentDefinitions};
pub use checkpoint::{
    build_checkpoint_save_tool, build_checkpoint_tools, CheckpointDiff, CheckpointList,
    CheckpointSave, CheckpointSaveCtx, CheckpointToolCtx,
};
pub use count_lines::CountLines;
pub use edit::EditTool;
pub use episodic_recall::{episodic_recall_summary, EpisodicRecall};
pub use estimate_tokens::EstimateTokens;
pub use facts::{
    facts_path, facts_summary, load_facts, search_facts, Fact, FactStore, ForgetFact, RecallFact,
    RememberFact, ScoredFact, UpdateFact,
};
#[cfg(feature = "skill-hub")]
pub use find_skills::FindSkills;
pub use fs::{ReadFile, WriteFile};
pub use glob::GlobTool;
#[cfg(feature = "skill-hub")]
pub use install_skill::InstallSkill;
pub use load_skill::LoadSkill;
pub use memory::{
    load_scratchpad, scratchpad_path, scratchpad_summary, Scratchpad, ScratchpadDelete,
    ScratchpadGet, ScratchpadList, WorkingMemoryTool,
};
pub use memory::{Forget, Recall, Remember};
pub use permission_pipeline::{CheckOutcome, PermissionPipeline};
pub use plan_mode::{
    EnterPlanModeTool, ExitPlanModeTool, PlanApprovalGate, PlanApprovalResult, PlanModeRequestGate,
    PlanModeRequestResult, RequestPlanModeTool,
};
pub use policy_sandbox::{FsPolicy, PolicyConfig, ShellPolicy};
pub use run_background::{BackgroundJobManager, CheckBackground, Job, JobState, RunBackground};
pub use run_skill_script::RunSkillScript;
pub use schedule_wakeup::{ScheduleWakeup, WakeupRequest, WakeupSlot};
pub use search::SearchFiles;
pub use send_message::{ListWorkersTool, SendMessageTool, WorkerMailbox, WorkerRegistry};
pub use shell::RunShell;

#[cfg(feature = "coordinator-mode")]
pub use task_create::TaskCreateTool;
#[cfg(feature = "coordinator-mode")]
pub use task_get::TaskGetTool;
#[cfg(feature = "coordinator-mode")]
pub use task_list::TaskListTool;
#[cfg(feature = "coordinator-mode")]
pub use task_output::TaskOutputTool;
#[cfg(feature = "coordinator-mode")]
pub use task_stop::TaskStopTool;
#[cfg(feature = "coordinator-mode")]
pub use task_update::TaskUpdateTool;
#[cfg(feature = "coordinator-mode")]
pub use team_create::TeamCreateTool;
#[cfg(feature = "coordinator-mode")]
pub use team_delete::TeamDeleteTool;
pub use todo::{TodoItem, TodoStatus, TodoWriteTool};
pub use tool_search::{DeferredCatalog, ToolSearchTool, TOOL_SEARCH_TOOL_NAME};
pub use transport::{DirEntry, ExecResult, LocalTransport, ReadResult, ToolTransport};
#[cfg(feature = "web_fetch")]
pub use web_fetch::WebFetch;
#[cfg(feature = "web_search")]
pub use web_search::WebSearch;

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::registry::{PermissionHook, Tool, ToolRegistry};
    use crate::error::{Error, Result};
    use crate::llm::ToolSpec;
    use crate::permissions::PermissionMode;
    use async_trait::async_trait;
    use serde_json::Value;
    use std::sync::Arc;

    struct Echo;

    #[async_trait]
    impl Tool for Echo {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "echo".into(),
                description: "echo".into(),
                parameters: serde_json::json!({"type":"object","properties":{"msg":{"type":"string"}}}),
            }
        }
        async fn execute(&self, args: Value) -> Result<String> {
            Ok(args["msg"].as_str().unwrap_or("").into())
        }
    }

    #[tokio::test]
    async fn registry_dispatches_and_errors_on_unknown() {
        let reg = ToolRegistry::local().register(Arc::new(Echo));
        let out = reg
            .invoke("echo", serde_json::json!({"msg":"hi"}))
            .await
            .unwrap();
        assert_eq!(out, "hi");
        let err = reg.invoke("nope", serde_json::json!({})).await.unwrap_err();
        assert!(matches!(err, Error::UnknownTool(_)));
    }

    #[test]
    fn resolve_within_rejects_escape() {
        let root = std::path::Path::new("/work");
        assert!(super::dispatch::resolve_within(root, "../etc/passwd").is_err());
        assert!(super::dispatch::resolve_within(root, "/elsewhere").is_err());
        assert!(super::dispatch::resolve_within(root, "src/lib.rs").is_ok());
    }

    #[test]
    fn resolve_within_handles_relative_root() {
        // Regression: `--workspace .` (relative) used to fail the prefix check.
        let cwd = std::env::current_dir().unwrap();
        let resolved =
            super::dispatch::resolve_within(std::path::Path::new("."), "src/lib.rs").unwrap();
        assert!(resolved.starts_with(&cwd));
        assert!(resolved.ends_with("src/lib.rs"));
    }

    #[tokio::test]
    async fn test_permission_deny_blocks_invoke() {
        let config = crate::permissions::LayeredPermissionsConfig {
            mode: PermissionMode::Default,
            layers: vec![crate::permissions::PermissionLayer {
                source: crate::permissions::RuleSource::User,
                allow: vec!["echo".into()],
                deny: vec!["echo".into()],
                ..Default::default()
            }],
        };
        let reg = ToolRegistry::local()
            .with_permissions(config)
            .register(Arc::new(Echo));
        let err = reg
            .invoke("echo", serde_json::json!({"msg":"hi"}))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::PermissionDenied { .. }));
    }

    // ── Goal-161: PermissionHook tests ───────────────────────────────────

    struct AllowHook;
    struct DenyHook;

    #[async_trait]
    impl PermissionHook for AllowHook {
        async fn check(
            &self,
            _name: &str,
            _args: &serde_json::Value,
        ) -> crate::agent::PermissionDecision {
            crate::agent::PermissionDecision::Allow
        }
    }

    #[async_trait]
    impl PermissionHook for DenyHook {
        async fn check(
            &self,
            _name: &str,
            _args: &serde_json::Value,
        ) -> crate::agent::PermissionDecision {
            crate::agent::PermissionDecision::Deny("denied by test hook".to_string())
        }
    }

    #[tokio::test]
    async fn permission_hook_allow_lets_tool_run() {
        let reg = ToolRegistry::local()
            .with_permission_hook(Arc::new(AllowHook))
            .register(Arc::new(Echo));
        let result = reg
            .invoke("echo", serde_json::json!({"msg": "hello"}))
            .await
            .unwrap();
        assert_eq!(result, "hello");
    }

    #[tokio::test]
    async fn permission_hook_deny_blocks_invoke() {
        let reg = ToolRegistry::local()
            .with_permission_hook(Arc::new(DenyHook))
            .register(Arc::new(Echo));
        let err = reg
            .invoke("echo", serde_json::json!({"msg": "blocked"}))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::PermissionDenied { .. }));
    }

    #[test]
    fn args_preview_truncates_long_strings() {
        let big_val = "x".repeat(200);
        let args = serde_json::json!({"command": big_val});
        let preview = super::dispatch::args_preview_for_permission(&args);
        assert!(preview.chars().count() <= 81); // 80 chars + ellipsis
    }

    // ── Goal-199: Headless mode tests ────────────────────────────────────

    /// headless=true, no hooks, interactive tool → PermissionDenied
    #[tokio::test]
    async fn headless_interactive_tool_denied_without_hooks() {
        let config = crate::permissions::LayeredPermissionsConfig {
            mode: PermissionMode::Default,
            layers: vec![crate::permissions::PermissionLayer {
                source: crate::permissions::RuleSource::User,
                interactive: vec!["echo".into()],
                ..Default::default()
            }],
        };
        let reg = ToolRegistry::local()
            .with_permissions(config)
            .with_headless(true)
            .register(Arc::new(Echo));
        let err = reg
            .invoke("echo", serde_json::json!({"msg": "hi"}))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::PermissionDenied { .. }));
    }

    /// headless=true, mock hook returns Continue → interactive tool allowed
    #[tokio::test]
    async fn headless_interactive_tool_allowed_by_hook() {
        use tempfile::tempdir;
        let tmp = tempdir().unwrap();
        let hook_path = tmp.path().join("allow.sh");
        let script = "#!/bin/sh\nread -r _\necho '{\"action\":\"continue\"}'\n";
        std::fs::write(&hook_path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&hook_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&hook_path, perms).unwrap();
        }
        let hook_runner = crate::hooks::ExternalHookRunner::discover(&[tmp.path().to_path_buf()]);

        let config = crate::permissions::LayeredPermissionsConfig {
            mode: PermissionMode::Default,
            layers: vec![crate::permissions::PermissionLayer {
                source: crate::permissions::RuleSource::User,
                interactive: vec!["echo".into()],
                ..Default::default()
            }],
        };
        let reg = ToolRegistry::local()
            .with_permissions(config)
            .with_headless(true)
            .with_hook_runner(hook_runner)
            .register(Arc::new(Echo));

        #[cfg(unix)]
        {
            let result = reg
                .invoke("echo", serde_json::json!({"msg": "allowed"}))
                .await
                .unwrap();
            assert_eq!(result, "allowed");
        }
        #[cfg(not(unix))]
        {
            let err = reg
                .invoke("echo", serde_json::json!({"msg": "blocked"}))
                .await
                .unwrap_err();
            assert!(matches!(err, Error::PermissionDenied { .. }));
        }
    }

    /// headless=false → interactive tools go through normal path (Passthrough)
    #[tokio::test]
    async fn non_headless_interactive_not_auto_denied() {
        let config = crate::permissions::LayeredPermissionsConfig {
            mode: PermissionMode::Default,
            layers: vec![crate::permissions::PermissionLayer {
                source: crate::permissions::RuleSource::User,
                interactive: vec!["echo".into()],
                ..Default::default()
            }],
        };
        let reg = ToolRegistry::local()
            .with_permissions(config)
            .with_headless(false)
            .register(Arc::new(Echo));
        let result = reg
            .invoke("echo", serde_json::json!({"msg": "hello"}))
            .await
            .unwrap();
        assert_eq!(result, "hello");
    }

    // ── Goal-212: Permission::Unknown semantics ──────────────────────────────

    /// Smoke test: Permission::Unknown variant compiles and can be constructed.
    #[test]
    fn permission_unknown_variant_exists() {
        use crate::permissions::Permission;
        let u = Permission::Unknown;
        assert!(!u.is_allowed());
        assert!(!u.is_denied());
    }

    /// Unknown + non-headless + interactive + DenyHook → PermissionDenied
    /// (invoke_with_audit called directly, bypassing invoke()).
    #[tokio::test]
    async fn unknown_interactive_tool_deny_hook_blocks_invoke_with_audit() {
        let config = crate::permissions::LayeredPermissionsConfig {
            mode: PermissionMode::Default,
            layers: vec![crate::permissions::PermissionLayer {
                source: crate::permissions::RuleSource::User,
                interactive: vec!["echo".into()],
                ..Default::default()
            }],
        };
        let reg = ToolRegistry::local()
            .with_permissions(config)
            .with_permission_hook(Arc::new(DenyHook))
            .with_headless(false)
            .register(Arc::new(Echo));
        let dispatch = reg
            .invoke_with_audit("echo", serde_json::json!({"msg": "hi"}))
            .await;
        assert!(
            matches!(dispatch.result, Err(Error::PermissionDenied { .. })),
            "hook-deny should block interactive Unknown tool via invoke_with_audit"
        );
    }

    /// Unknown + non-headless + interactive + no hook → allowed (library default).
    #[tokio::test]
    async fn unknown_interactive_tool_no_hook_is_allowed() {
        let config = crate::permissions::LayeredPermissionsConfig {
            mode: PermissionMode::Default,
            layers: vec![crate::permissions::PermissionLayer {
                source: crate::permissions::RuleSource::User,
                interactive: vec!["echo".into()],
                ..Default::default()
            }],
        };
        let reg = ToolRegistry::local()
            .with_permissions(config)
            .with_headless(false)
            .register(Arc::new(Echo));
        let dispatch = reg
            .invoke_with_audit("echo", serde_json::json!({"msg": "ok"}))
            .await;
        assert_eq!(
            dispatch.result.unwrap(),
            "ok",
            "no hook = allow for Unknown interactive tool"
        );
    }

    /// Unknown + non-headless + non-interactive + DenyHook → allowed (hook not consulted).
    #[tokio::test]
    async fn unknown_non_interactive_tool_hook_not_consulted() {
        // echo is NOT in the interactive list; DenyHook should not fire.
        let config = crate::permissions::LayeredPermissionsConfig {
            mode: PermissionMode::Default,
            layers: vec![],
        };
        let reg = ToolRegistry::local()
            .with_permissions(config)
            .with_permission_hook(Arc::new(DenyHook))
            .with_headless(false)
            .register(Arc::new(Echo));
        let dispatch = reg
            .invoke_with_audit("echo", serde_json::json!({"msg": "pass"}))
            .await;
        assert_eq!(
            dispatch.result.unwrap(),
            "pass",
            "DenyHook must not fire for non-interactive Unknown tools"
        );
    }

    /// Allowed (explicit allow rule) + interactive + DenyHook
    /// → allowed (no hook fired because perm_is_unknown=false).
    #[tokio::test]
    async fn allowed_interactive_tool_hook_not_consulted() {
        let config = crate::permissions::LayeredPermissionsConfig {
            mode: PermissionMode::Default,
            layers: vec![crate::permissions::PermissionLayer {
                source: crate::permissions::RuleSource::User,
                allow: vec!["echo".into()],
                interactive: vec!["echo".into()],
                ..Default::default()
            }],
        };
        let reg = ToolRegistry::local()
            .with_permissions(config)
            .with_permission_hook(Arc::new(DenyHook))
            .with_headless(false)
            .register(Arc::new(Echo));
        // invoke_with_audit directly: check_static returns Allowed, not Unknown,
        // so the Goal-212 hook block must NOT fire.
        let dispatch = reg
            .invoke_with_audit("echo", serde_json::json!({"msg": "explicit-allow"}))
            .await;
        assert_eq!(
            dispatch.result.unwrap(),
            "explicit-allow",
            "Explicitly Allowed tools must not be re-checked via hook"
        );
    }

    // ── Goal-201: plan mode tools are opt-in (not in default registry) ──────

    #[test]
    fn default_registry_has_no_plan_mode_tools() {
        // build_standard_tools() must NOT register enter_plan_mode / exit_plan_mode.
        // These are channel capabilities owned exclusively by AgentRuntimeBuilder.
        let workspace = std::path::PathBuf::from(".");
        let registry = super::registry::build_standard_tools(&workspace, &[], 30);
        assert!(
            registry.get("enter_plan_mode").is_none(),
            "enter_plan_mode must not be in the default registry"
        );
        assert!(
            registry.get("exit_plan_mode").is_none(),
            "exit_plan_mode must not be in the default registry"
        );
    }

    // ── Goal-247: ToolRegistry::fork() ──────────────────────────────────────

    #[test]
    fn fork_returns_usable_registry() {
        let reg = ToolRegistry::local().register(Arc::new(Echo));
        let forked = reg.fork();
        // fork() should return a registry that can invoke tools
        assert!(
            forked.get("echo").is_some(),
            "forked registry should contain 'echo'"
        );
        assert!(
            forked.get("nope").is_none(),
            "forked registry should not contain unknown tools"
        );
    }
}
