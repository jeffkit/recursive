//! `spawn_worker` tool: first-class coordinator-pattern delegation.
//!
//! Unlike the lower-level `sub_agent` tool (which the LLM can use for arbitrary
//! sub-tasks), `spawn_worker` is designed for the **coordinator pattern** where
//! a lead agent explicitly delegates structured work to specialist workers.
//!
//! Motivation: The old `TeamOrchestrator` approach required the LLM to write
//! `DELEGATE:<role>:<task>` text strings, which were then parsed — a fragile,
//! non-type-safe mechanism. `spawn_worker` gives the coordinator agent a proper
//! JSON tool call to express delegation, eliminating text parsing entirely.
//!
//! # Worker types
//!
//! | `worker_type` | System prompt focus | Tool access |
//! |---------------|---------------------|-------------|
//! | `general`     | General-purpose     | Full (parent registry) |
//! | `explore`     | Read-only research  | Read, Grep, WebFetch |
//! | `coder`       | Code implementation | Full |
//! | `reviewer`    | Code review         | Read, Grep |
//! | `researcher`  | External research   | Read, Grep, WebFetch |

use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::agent::FinishReason;
use crate::error::{Error, Result};
use crate::kernel::{AgentKernel, TurnContext};
use crate::llm::{LlmProvider, ToolSpec};
use crate::message::Message;
use crate::multi::AgentPool;
use crate::permissions::PermissionMode;
use crate::tools::team_manage::{SharedMemoryRead, SharedMemoryWrite};
use crate::tools::PermissionHook;
use crate::tools::{Tool, ToolRegistry, ToolSideEffect};

// ---------------------------------------------------------------------------
// WorkerType
// ---------------------------------------------------------------------------

/// Named worker personality for coordinator-pattern delegation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerType {
    General,
    Explore,
    Coder,
    Reviewer,
    Researcher,
}

impl WorkerType {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "general" => Some(Self::General),
            "explore" => Some(Self::Explore),
            "coder" => Some(Self::Coder),
            "reviewer" => Some(Self::Reviewer),
            "researcher" => Some(Self::Researcher),
            _ => None,
        }
    }

    /// Whether this worker type only reads (never writes files).
    pub fn is_read_only(self) -> bool {
        matches!(self, Self::Explore | Self::Reviewer | Self::Researcher)
    }

    pub fn system_prompt(self) -> &'static str {
        match self {
            Self::General => {
                "You are a focused worker agent. Complete the given task using the \
                 available tools. Be concise and report what you did."
            }
            Self::Explore => {
                "You are a read-only exploration agent. Use only read tools to gather \
                 information about the codebase or files. Do NOT write or modify any files. \
                 Report findings clearly and concisely."
            }
            Self::Coder => {
                "You are a coding specialist. Implement the requested feature or fix. \
                 Write clean, tested code. Run cargo test or equivalent after changes. \
                 Commit your work and report the result."
            }
            Self::Reviewer => {
                "You are a code reviewer. Read the relevant code carefully and provide \
                 a structured review: summary of changes, potential issues, and suggestions. \
                 Do NOT modify any files."
            }
            Self::Researcher => {
                "You are a research specialist. Gather information from files, search results, \
                 or web sources to answer the question or produce a report. \
                 Do NOT modify any files. Report findings in a structured format."
            }
        }
    }

    /// Restricted tool names for this worker type, or `None` for full access.
    pub fn allowed_tool_names(self) -> Option<Vec<String>> {
        match self {
            Self::General | Self::Coder => None,
            Self::Explore => Some(vec![
                "Read".to_string(),
                "Grep".to_string(),
                "WebFetch".to_string(),
            ]),
            Self::Reviewer => Some(vec!["Read".to_string(), "Grep".to_string()]),
            Self::Researcher => Some(vec![
                "Read".to_string(),
                "Grep".to_string(),
                "WebFetch".to_string(),
            ]),
        }
    }
}

// ---------------------------------------------------------------------------
// SpawnWorkerTool
// ---------------------------------------------------------------------------

/// The `spawn_worker` tool — a first-class coordinator delegation mechanism.
pub struct SpawnWorkerTool {
    workspace: std::path::PathBuf,
    provider: Arc<dyn LlmProvider>,
    all_tools: ToolRegistry,
    max_depth: usize,
    current_depth: usize,
    permission_hook: Option<Arc<dyn PermissionHook>>,
    /// Optional registry — when set, each spawned worker is registered so
    /// a coordinator can send mid-run messages via `send_message`.
    registry: Option<crate::tools::send_message::WorkerRegistry>,
    /// Optional agent pool — when set, `role_name` parameter can look up
    /// custom roles defined via `team_add_role`.
    pool: Option<Arc<RwLock<AgentPool>>>,
}

impl SpawnWorkerTool {
    pub fn new(
        workspace: impl Into<std::path::PathBuf>,
        provider: Arc<dyn LlmProvider>,
        all_tools: ToolRegistry,
        max_depth: usize,
        current_depth: usize,
        permission_hook: Option<Arc<dyn PermissionHook>>,
    ) -> Self {
        Self {
            workspace: workspace.into(),
            provider,
            all_tools,
            max_depth,
            current_depth,
            permission_hook,
            registry: None,
            pool: None,
        }
    }

    /// Attach a `WorkerRegistry` so spawned workers can receive mid-run messages.
    pub fn with_registry(mut self, registry: crate::tools::send_message::WorkerRegistry) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Attach an `AgentPool` so spawned workers can use custom roles via `role_name`.
    pub fn with_pool(mut self, pool: Arc<RwLock<AgentPool>>) -> Self {
        self.pool = Some(pool);
        self
    }

    fn build_sub_registry(&self, tool_names: &[String]) -> ToolRegistry {
        let mut reg = self.all_tools.with_same_transport();
        for name in tool_names {
            if let Some(tool) = self.all_tools.get(name) {
                reg = reg.register(tool);
            }
        }
        reg
    }
}

#[async_trait]
impl Tool for SpawnWorkerTool {
    fn is_deferred(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "spawn_worker".into(),
            description: concat!(
                "Spawn a specialist worker agent to handle a focused sub-task. ",
                "This is the coordinator-pattern delegation tool: use it to assign work to ",
                "a named specialist (explore, coder, reviewer, researcher, or general), ",
                "or to a custom role defined via team_add_role (use role_name). ",
                "The worker runs independently with an empty transcript and returns its result."
            )
            .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "Complete task description for the worker. Include all necessary context — the worker has no access to this conversation."
                    },
                    "worker_type": {
                        "type": "string",
                        "enum": ["general", "explore", "coder", "reviewer", "researcher"],
                        "description": "Specialist type: 'explore'/'reviewer'/'researcher' are read-only; 'coder'/'general' have full tool access. Ignored if role_name is set.",
                        "default": "general"
                    },
                    "role_name": {
                        "type": "string",
                        "description": "Optional: name of a custom role defined via team_add_role. When set, the role's system_prompt, max_steps, and allowed_tools override worker_type defaults."
                    },
                    "system_prompt": {
                        "type": "string",
                        "description": "Optional: override the system prompt (takes precedence over both worker_type and role_name defaults)."
                    },
                    "max_steps": {
                        "type": "integer",
                        "description": "Maximum steps for the worker (default 30, max 100). Role's max_steps used when role_name is set and this is not specified.",
                        "default": 30
                    }
                },
                "required": ["prompt"]
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        // Read-only workers are safe to run in parallel; general/coder workers
        // may write files, so they are External (sequential by default).
        ToolSideEffect::External
    }

    /// Allow parallel dispatch when the worker type is read-only.
    fn is_readonly_for_args(&self, arguments: &Value) -> bool {
        arguments
            .get("worker_type")
            .and_then(|v| v.as_str())
            .and_then(WorkerType::parse)
            .map(WorkerType::is_read_only)
            .unwrap_or(false)
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let prompt = arguments["prompt"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "spawn_worker".into(),
                message: "missing required parameter: prompt".to_string(),
            })?;

        // Depth limit check (reuse same env var as sub_agent)
        if self.current_depth >= self.max_depth {
            return Ok(format!(
                "ERROR: worker depth limit reached (max_depth={}). Cannot spawn deeper worker.",
                self.max_depth
            ));
        }

        // Resolve role config: role_name (custom pool role) > worker_type (preset)
        let role_name_opt = arguments.get("role_name").and_then(|v| v.as_str());

        // Look up custom role from pool if role_name is provided
        let pool_role = if let (Some(role_name), Some(pool)) = (role_name_opt, &self.pool) {
            let pool_guard = pool.read().await;
            pool_guard.get_role(role_name).map(|r| {
                (
                    r.system_prompt.clone(),
                    r.max_steps,
                    r.allowed_tools.clone(),
                )
            })
        } else {
            None
        };

        let worker_type = arguments
            .get("worker_type")
            .and_then(|v| v.as_str())
            .and_then(WorkerType::parse)
            .unwrap_or(WorkerType::General);

        // max_steps priority: explicit arg > pool role default > 30
        let max_steps = if arguments
            .get("max_steps")
            .and_then(|v| v.as_i64())
            .is_some()
        {
            arguments["max_steps"].as_i64().unwrap_or(30).clamp(1, 100) as usize
        } else if let Some((_, role_max_steps, _)) = &pool_role {
            (*role_max_steps).min(100)
        } else {
            30
        };

        // System prompt priority: explicit arg > pool role > worker_type default
        let base_prompt = if let Some(override_prompt) =
            arguments.get("system_prompt").and_then(|v| v.as_str())
        {
            override_prompt.to_string()
        } else if let Some((role_prompt, _, _)) = &pool_role {
            role_prompt.clone()
        } else {
            worker_type.system_prompt().to_string()
        };

        // Gap-1: Inject SharedMemory context from the pool so workers can see
        // state published by other workers or the coordinator.
        let memory_ctx = if let Some(pool) = &self.pool {
            pool.read().await.memory().to_context_string().await
        } else {
            String::new()
        };
        let sys_prompt = if memory_ctx.is_empty() {
            base_prompt
        } else {
            format!("{base_prompt}\n\n{memory_ctx}")
        };

        // Assign a stable worker_id and optionally register in the registry so
        // the coordinator can send mid-run messages via `send_message`.
        let worker_id = arguments
            .get("worker_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        let mailbox = match &self.registry {
            Some(reg) => Some(reg.register(&worker_id).await),
            None => None,
        };

        // Build the tool registry for this worker.
        // Priority: pool role's allowed_tools > worker_type's allowed_tool_names.
        let role_tools = pool_role.as_ref().and_then(|(_, _, tools)| {
            if tools.is_empty() {
                None
            } else {
                Some(tools.clone())
            }
        });
        let tool_names = role_tools.or_else(|| worker_type.allowed_tool_names());

        let mut sub_registry = match &tool_names {
            Some(names) => self.build_sub_registry(names),
            None => {
                // Full access: give worker all parent tools.
                // Also register a child spawn_worker so workers can sub-delegate.
                let mut reg = self.all_tools.clone();
                let mut child = SpawnWorkerTool::new(
                    &self.workspace,
                    self.provider.clone(),
                    self.all_tools.clone(),
                    self.max_depth,
                    self.current_depth + 1,
                    self.permission_hook.clone(),
                );
                if let Some(r) = &self.registry {
                    child = child.with_registry(r.clone());
                }
                if let Some(p) = &self.pool {
                    child = child.with_pool(p.clone());
                }
                reg = reg.register(Arc::new(child));
                reg
            }
        };

        // Gap-2: Register shared memory tools when pool is available so workers can
        // publish intermediate results and read state from other workers.
        if let Some(pool) = &self.pool {
            let memory = pool.read().await.memory().clone();
            let author = role_name_opt
                .map(str::to_string)
                .unwrap_or_else(|| worker_id.clone());
            sub_registry = sub_registry
                .register(Arc::new(SharedMemoryWrite::new(memory.clone(), author)))
                .register(Arc::new(SharedMemoryRead::new(memory)));
        }

        let kernel = AgentKernel::builder()
            .llm(self.provider.clone())
            .tools(sub_registry)
            .max_steps(max_steps)
            .build()
            .map_err(|e| Error::Tool {
                name: "spawn_worker".into(),
                message: format!("failed to build worker kernel: {e}"),
            })?;

        let ctx = TurnContext {
            messages: vec![
                Message::system(sys_prompt),
                Message::user(prompt.to_string()),
            ],
            step_events_tx: None,
            tool_specs: kernel.tools().specs(),
            streaming: false,
            permission_hook: self.permission_hook.clone(),
            exploring_plan_mode: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            permission_mode: PermissionMode::Default,
            mailbox,
        };

        let outcome = kernel.run(ctx).await;

        // Deregister from the registry once the worker is done (success or error).
        if let Some(reg) = &self.registry {
            reg.deregister(&worker_id).await;
        }

        let outcome = outcome.map_err(|e| Error::Tool {
            name: "spawn_worker".into(),
            message: format!("worker failed: {e}"),
        })?;

        let finish_label = match &outcome.finish_reason {
            FinishReason::NoMoreToolCalls => "NoMoreToolCalls",
            FinishReason::BudgetExceeded => "BudgetExceeded",
            FinishReason::ProviderStop(r) => r,
            FinishReason::Stuck { .. } => "Stuck",
            FinishReason::TranscriptLimit { .. } => "TranscriptLimit",
            FinishReason::Cancelled => "Cancelled",
            FinishReason::PermissionDenialLimit => "PermissionDenialLimit",
        };

        let final_text = outcome
            .final_text
            .unwrap_or_else(|| "(no final message)".to_string());

        let type_label = if let Some(role_name) = role_name_opt {
            role_name.to_string()
        } else {
            match worker_type {
                WorkerType::General => "general".to_string(),
                WorkerType::Explore => "explore".to_string(),
                WorkerType::Coder => "coder".to_string(),
                WorkerType::Reviewer => "reviewer".to_string(),
                WorkerType::Researcher => "researcher".to_string(),
            }
        };

        Ok(format!(
            "[worker_id:{worker_id} type:{type_label} finished:{finish_label}]\n{final_text}"
        ))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Completion, MockProvider};
    use crate::tools::{LocalTransport, ReadFile, SearchFiles, ToolTransport, WriteFile};
    use tempfile::TempDir;

    fn mock_provider(script: Vec<Completion>) -> Arc<dyn LlmProvider> {
        Arc::new(MockProvider::new(script))
    }

    fn full_registry(workspace: &std::path::Path) -> ToolRegistry {
        let transport: Arc<dyn ToolTransport> = Arc::new(LocalTransport);
        ToolRegistry::new(transport)
            .register(Arc::new(ReadFile::new(workspace)))
            .register(Arc::new(WriteFile::new(workspace)))
            .register(Arc::new(SearchFiles::new(workspace)))
    }

    fn make_tool(workspace: &std::path::Path, provider: Arc<dyn LlmProvider>) -> SpawnWorkerTool {
        let registry = full_registry(workspace);
        SpawnWorkerTool::new(workspace, provider, registry, 2, 0, None)
    }

    #[tokio::test]
    async fn spawn_worker_general_type() {
        let dir = TempDir::new().unwrap();
        let provider = mock_provider(vec![Completion {
            content: "Task complete. I analysed the situation.".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]);
        let tool = make_tool(dir.path(), provider);
        let result = tool
            .execute(json!({"prompt": "Summarise the current directory."}))
            .await
            .unwrap();
        assert!(result.contains("type:general finished:"), "got: {result}");
        assert!(result.contains("Task complete"), "got: {result}");
    }

    #[tokio::test]
    async fn spawn_worker_explore_type() {
        let dir = TempDir::new().unwrap();
        let provider = mock_provider(vec![Completion {
            content: "Found 3 Rust source files.".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]);
        let tool = make_tool(dir.path(), provider);
        let result = tool
            .execute(json!({"prompt": "List Rust files.", "worker_type": "explore"}))
            .await
            .unwrap();
        assert!(result.contains("type:explore finished:"), "got: {result}");
    }

    #[tokio::test]
    async fn spawn_worker_custom_system_prompt() {
        let dir = TempDir::new().unwrap();
        let provider = mock_provider(vec![Completion {
            content: "Custom result".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]);
        let tool = make_tool(dir.path(), provider);
        let result = tool
            .execute(json!({
                "prompt": "Do something.",
                "worker_type": "coder",
                "system_prompt": "You are a custom expert."
            }))
            .await
            .unwrap();
        // Should succeed and use the coder worker type label
        assert!(result.contains("type:coder finished:"), "got: {result}");
    }

    #[tokio::test]
    async fn spawn_worker_missing_prompt() {
        let dir = TempDir::new().unwrap();
        let provider = mock_provider(vec![]);
        let tool = make_tool(dir.path(), provider);
        let result = tool.execute(json!({"worker_type": "general"})).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("missing required parameter"),
            "got: {err}"
        );
    }
}
