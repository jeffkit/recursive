//! Tool abstraction: any side effect the model can request.
//!
//! Tools are orthogonal to the agent and to each other. To add a capability
//! you implement `Tool` and register it; no other file changes.

use async_trait::async_trait;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use tracing::Instrument;

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::permissions::{Permission, PermissionsConfig};

pub mod apply_patch;
pub mod episodic_recall;
pub mod estimate_tokens;
pub mod facts;
pub mod fs;
pub mod load_skill;
pub mod memory;
pub mod run_background;
pub mod run_skill_script;
pub mod schedule_wakeup;
pub mod search;
pub mod shell;
pub mod sub_agent;
pub mod transport;
#[cfg(feature = "web_fetch")]
pub mod web_fetch;

pub use apply_patch::ApplyPatch;
pub use episodic_recall::{episodic_recall_summary, EpisodicRecall};
pub use estimate_tokens::EstimateTokens;
pub use facts::{
    facts_path, facts_summary, load_facts, search_facts, Fact, FactStore, ForgetFact, RecallFact,
    RememberFact, ScoredFact, UpdateFact,
};
pub use fs::{ListDir, ReadFile, WriteFile};
pub use load_skill::LoadSkill;
pub use memory::{
    load_scratchpad, scratchpad_path, scratchpad_summary, Scratchpad, ScratchpadDelete,
    ScratchpadGet, ScratchpadList, WorkingMemoryTool,
};
pub use memory::{Forget, Recall, Remember};
pub use run_background::{BackgroundJobManager, CheckBackground, Job, JobState, RunBackground};
pub use run_skill_script::RunSkillScript;
pub use schedule_wakeup::{ScheduleWakeup, WakeupRequest, WakeupSlot};
pub use search::SearchFiles;
pub use shell::RunShell;
pub use sub_agent::SubAgent;
pub use transport::{DirEntry, ExecResult, LocalTransport, ReadResult, ToolTransport};
#[cfg(feature = "web_fetch")]
pub use web_fetch::WebFetch;

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::local()
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn execute(&self, arguments: Value) -> Result<String>;
    /// Whether this tool only reads data without side effects.
    /// Default: false (conservative). Override to true for read-only tools.
    fn is_readonly(&self) -> bool {
        false
    }
}

#[derive(Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    transport: Arc<dyn ToolTransport>,
    permissions: Option<PermissionsConfig>,
}

impl ToolRegistry {
    pub fn new(transport: Arc<dyn ToolTransport>) -> Self {
        Self {
            tools: BTreeMap::new(),
            transport,
            permissions: None,
        }
    }

    /// Create a registry with the default local transport.
    pub fn local() -> Self {
        Self::new(Arc::new(LocalTransport))
    }

    /// Returns a reference to the transport layer.
    pub fn transport(&self) -> &Arc<dyn ToolTransport> {
        &self.transport
    }

    /// Create a new empty registry that shares the same transport.
    pub fn with_same_transport(&self) -> Self {
        Self {
            tools: BTreeMap::new(),
            transport: self.transport.clone(),
            permissions: self.permissions.clone(),
        }
    }

    /// Set the permissions configuration for this registry.
    pub fn with_permissions(mut self, permissions: PermissionsConfig) -> Self {
        self.permissions = Some(permissions);
        self
    }

    pub fn register(mut self, tool: Arc<dyn Tool>) -> Self {
        let name = tool.spec().name;
        self.tools.insert(name, tool);
        self
    }

    /// Register a tool via mutable reference (for use with shared registries).
    pub fn register_mut(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.spec().name;
        self.tools.insert(name, tool);
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec()).collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Check if a tool is read-only (no side effects).
    pub fn is_readonly(&self, name: &str) -> bool {
        self.tools
            .get(name)
            .map(|t| t.is_readonly())
            .unwrap_or(false)
    }

    pub async fn invoke(&self, name: &str, arguments: Value) -> Result<String> {
        // Static permission check before any tool execution
        if let Some(ref config) = self.permissions {
            match config.check_static(name) {
                Permission::Denied(_reason) => {
                    return Err(Error::PermissionDenied { name: name.into() });
                }
                Permission::Allowed => {}
            }
        }

        let args_size = arguments.to_string().len();
        let span = tracing::info_span!("tool.execute", name = %name, args_size);
        async move {
            let tool = self
                .get(name)
                .ok_or_else(|| Error::UnknownTool(name.into()))?;
            tool.execute(arguments).await.map_err(|e| match e {
                Error::Tool { .. } | Error::BadToolArgs { .. } | Error::UnknownTool(_) => e,
                other => Error::Tool {
                    name: name.into(),
                    message: other.to_string(),
                },
            })
        }
        .instrument(span)
        .await
    }
}

/// Resolve a possibly-relative path against the workspace root.
///
/// Both the root and the candidate are normalised to an absolute, dot-free
/// form before comparison so that `--workspace .` works exactly the same as
/// `--workspace /abs/path`.
pub(crate) fn resolve_within(root: &std::path::Path, path: &str) -> Result<std::path::PathBuf> {
    let candidate = std::path::Path::new(path);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    let abs_root = absolutise(root);
    let abs_joined = absolutise(&joined);
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
    use crate::permissions::PermissionsConfig;
    use async_trait::async_trait;

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
        assert!(resolve_within(root, "../etc/passwd").is_err());
        assert!(resolve_within(root, "/elsewhere").is_err());
        assert!(resolve_within(root, "src/lib.rs").is_ok());
    }

    #[test]
    fn resolve_within_handles_relative_root() {
        // Regression: `--workspace .` (relative) used to fail the prefix check.
        let cwd = std::env::current_dir().unwrap();
        let resolved = resolve_within(std::path::Path::new("."), "src/lib.rs").unwrap();
        assert!(resolved.starts_with(&cwd));
        assert!(resolved.ends_with("src/lib.rs"));
    }

    #[tokio::test]
    async fn test_permission_deny_blocks_invoke() {
        let config = PermissionsConfig {
            allow: vec!["echo".into()],
            deny: vec!["echo".into()],
            interactive: vec![],
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
}
