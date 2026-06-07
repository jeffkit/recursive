//! `spawn_workers_parallel` tool: dispatch multiple workers concurrently.
//!
//! Unlike `spawn_worker` which runs one worker sequentially, this tool accepts
//! an array of task descriptors and runs all of them in parallel using
//! `futures::future::join_all`. Results are collected and returned together,
//! significantly reducing latency for independent sub-tasks.
//!
//! # Usage
//!
//! ```json
//! {
//!   "tasks": [
//!     {"prompt": "Analyse the auth module", "worker_type": "explore"},
//!     {"prompt": "Analyse the API handlers", "worker_type": "explore"},
//!     {"prompt": "Review the error handling", "worker_type": "reviewer"}
//!   ]
//! }
//! ```
//!
//! All read-only worker types (explore, reviewer, researcher) run truly in
//! parallel. Tasks with `worker_type: "coder"` or `"general"` are also
//! spawned concurrently — callers must ensure tasks don't conflict on files.

use async_trait::async_trait;
use futures_util::future::join_all;
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
use crate::tools::send_message::{ListWorkersTool, SendMessageTool, WorkerMailbox, WorkerRegistry};
use crate::tools::spawn_worker::WorkerType;
use crate::tools::team_manage::{SharedMemoryRead, SharedMemoryWrite};
use crate::tools::PermissionHook;
use crate::tools::{Tool, ToolRegistry, ToolSideEffect};

/// A single task descriptor inside a `spawn_workers_parallel` call.
#[derive(Debug, Clone)]
struct TaskDescriptor {
    prompt: String,
    worker_type: WorkerType,
    role_name: Option<String>,
    system_prompt_override: Option<String>,
    max_steps: usize,
    worker_id: Option<String>,
}

impl TaskDescriptor {
    fn from_value(v: &Value) -> std::result::Result<Self, String> {
        let prompt = v["prompt"]
            .as_str()
            .ok_or("missing required field: prompt")?
            .to_string();

        let worker_type = v
            .get("worker_type")
            .and_then(|t| t.as_str())
            .and_then(WorkerType::parse)
            .unwrap_or(WorkerType::General);

        let role_name = v
            .get("role_name")
            .and_then(|t| t.as_str())
            .map(str::to_string);
        let system_prompt_override = v
            .get("system_prompt")
            .and_then(|t| t.as_str())
            .map(str::to_string);
        let max_steps = v
            .get("max_steps")
            .and_then(|t| t.as_i64())
            .map(|n| n.clamp(1, 100) as usize)
            .unwrap_or(30);
        let worker_id = v
            .get("worker_id")
            .and_then(|t| t.as_str())
            .map(str::to_string);

        Ok(Self {
            prompt,
            worker_type,
            role_name,
            system_prompt_override,
            max_steps,
            worker_id,
        })
    }
}

/// The `spawn_workers_parallel` tool.
pub struct SpawnWorkersParallel {
    #[allow(dead_code)]
    workspace: std::path::PathBuf,
    provider: Arc<dyn LlmProvider>,
    all_tools: ToolRegistry,
    max_depth: usize,
    current_depth: usize,
    permission_hook: Option<Arc<dyn PermissionHook>>,
    pool: Option<Arc<RwLock<AgentPool>>>,
    /// Optional registry for inter-worker messaging via `send_message`.
    registry: Option<WorkerRegistry>,
}

impl SpawnWorkersParallel {
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
            pool: None,
            registry: None,
        }
    }

    /// Attach an `AgentPool` so workers can use custom roles via `role_name`.
    pub fn with_pool(mut self, pool: Arc<RwLock<AgentPool>>) -> Self {
        self.pool = Some(pool);
        self
    }

    /// Attach a `WorkerRegistry` for inter-worker messaging.
    /// When set, all parallel workers are registered and given `send_message` +
    /// `list_workers` tools so they can communicate with each other mid-run.
    pub fn with_registry(mut self, registry: WorkerRegistry) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Build a restricted tool registry containing only the named tools.
    ///
    /// Uses `with_same_transport()` (empty tool set, shared transport/policy)
    /// rather than `fork()` (which cloned all tools), so workers actually
    /// run with only the declared tool subset.
    fn build_sub_registry(&self, tool_names: &[String]) -> ToolRegistry {
        let mut reg = self.all_tools.with_same_transport();
        for name in tool_names {
            if let Some(tool) = self.all_tools.get(name) {
                reg = reg.register(tool);
            }
        }
        reg
    }

    /// Run a single task and return `(index, result_string)`.
    async fn run_task(
        &self,
        index: usize,
        task: TaskDescriptor,
        pool_role: Option<(String, usize, Vec<String>)>,
        mailbox: Option<WorkerMailbox>,
    ) -> (usize, String) {
        // Resolve system prompt
        let sys_prompt = if let Some(override_p) = &task.system_prompt_override {
            override_p.clone()
        } else if let Some((role_prompt, _, _)) = &pool_role {
            role_prompt.clone()
        } else {
            task.worker_type.system_prompt().to_string()
        };

        // Resolve max_steps
        let max_steps = if task.max_steps != 30 {
            task.max_steps
        } else if let Some((_, role_steps, _)) = &pool_role {
            (*role_steps).min(100)
        } else {
            task.max_steps
        };

        // Resolve system prompt: inject SharedMemory context if pool is set
        let memory_ctx = if let Some(pool) = &self.pool {
            pool.read().await.memory().to_context_string().await
        } else {
            String::new()
        };
        let sys_prompt = if memory_ctx.is_empty() {
            sys_prompt
        } else {
            format!("{sys_prompt}\n\n{memory_ctx}")
        };

        // Resolve tool access
        let role_tools = pool_role.as_ref().and_then(|(_, _, tools)| {
            if tools.is_empty() {
                None
            } else {
                Some(tools.clone())
            }
        });
        let tool_names = role_tools.or_else(|| task.worker_type.allowed_tool_names());

        let mut sub_registry = match &tool_names {
            Some(names) => self.build_sub_registry(names),
            None => self.all_tools.clone(),
        };

        // Gap-2 (parallel): inject SharedMemory read/write tools when pool is available
        if let Some(pool) = &self.pool {
            let memory = pool.read().await.memory().clone();
            let author = task
                .role_name
                .clone()
                .or_else(|| task.worker_id.clone())
                .unwrap_or_else(|| index.to_string());
            sub_registry = sub_registry
                .register(Arc::new(SharedMemoryWrite::new(memory.clone(), author)))
                .register(Arc::new(SharedMemoryRead::new(memory)));
        }

        // Gap-4: inject send_message + list_workers tools for inter-worker communication
        if let Some(reg) = &self.registry {
            sub_registry = sub_registry
                .register(Arc::new(SendMessageTool::new(reg.clone())))
                .register(Arc::new(ListWorkersTool::new(reg.clone())));
        }

        let kernel = match AgentKernel::builder()
            .llm(self.provider.clone())
            .tools(sub_registry)
            .max_steps(max_steps)
            .build()
        {
            Ok(k) => k,
            Err(e) => {
                return (index, format!("[task:{index} error:build_failed] {e}"));
            }
        };

        let ctx = TurnContext {
            messages: vec![
                Message::system(sys_prompt),
                Message::user(task.prompt.clone()),
            ],
            step_events_tx: None,
            tool_specs: kernel.tools().specs(),
            streaming: false,
            permission_hook: self.permission_hook.clone(),
            exploring_plan_mode: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            permission_mode: PermissionMode::Default,
            mailbox,
        };

        let outcome = match kernel.run(ctx).await {
            Ok(o) => o,
            Err(e) => {
                return (index, format!("[task:{index} error:run_failed] {e}"));
            }
        };

        let worker_id = task.worker_id.unwrap_or_else(|| index.to_string());

        let type_label = if let Some(role_name) = &task.role_name {
            role_name.clone()
        } else {
            match task.worker_type {
                WorkerType::General => "general".to_string(),
                WorkerType::Explore => "explore".to_string(),
                WorkerType::Coder => "coder".to_string(),
                WorkerType::Reviewer => "reviewer".to_string(),
                WorkerType::Researcher => "researcher".to_string(),
            }
        };

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

        (
            index,
            format!(
                "[worker_id:{worker_id} type:{type_label} finished:{finish_label}]\n{final_text}"
            ),
        )
    }
}

#[async_trait]
impl Tool for SpawnWorkersParallel {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "spawn_workers_parallel".into(),
            description: concat!(
                "Spawn multiple specialist workers in PARALLEL to handle independent sub-tasks. ",
                "All workers start at the same time and results are collected when all finish. ",
                "Use this when you have several independent tasks that don't need each other's output. ",
                "For sequential pipelines where each task depends on the previous, use spawn_worker in sequence."
            )
            .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "description": "Array of task descriptors to run in parallel.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "prompt": {
                                    "type": "string",
                                    "description": "Complete task description for this worker. Include all necessary context."
                                },
                                "worker_type": {
                                    "type": "string",
                                    "enum": ["general", "explore", "coder", "reviewer", "researcher"],
                                    "description": "Specialist type. Ignored if role_name is set.",
                                    "default": "general"
                                },
                                "role_name": {
                                    "type": "string",
                                    "description": "Optional: custom role name defined via team_add_role."
                                },
                                "system_prompt": {
                                    "type": "string",
                                    "description": "Optional: override the system prompt for this specific worker."
                                },
                                "max_steps": {
                                    "type": "integer",
                                    "description": "Maximum steps for this worker (default 30, max 100).",
                                    "default": 30
                                },
                                "worker_id": {
                                    "type": "string",
                                    "description": "Optional stable identifier shown in the result header."
                                }
                            },
                            "required": ["prompt"]
                        },
                        "minItems": 1
                    }
                },
                "required": ["tasks"]
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::External
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        if self.current_depth >= self.max_depth {
            return Ok(format!(
                "ERROR: worker depth limit reached (max_depth={}). Cannot spawn parallel workers.",
                self.max_depth
            ));
        }

        let tasks_raw = arguments["tasks"]
            .as_array()
            .ok_or_else(|| Error::BadToolArgs {
                name: "spawn_workers_parallel".into(),
                message: "missing required parameter: tasks (must be an array)".to_string(),
            })?;

        if tasks_raw.is_empty() {
            return Err(Error::BadToolArgs {
                name: "spawn_workers_parallel".into(),
                message: "tasks array must not be empty".to_string(),
            });
        }

        // Parse task descriptors
        let mut tasks = Vec::with_capacity(tasks_raw.len());
        for (i, t) in tasks_raw.iter().enumerate() {
            match TaskDescriptor::from_value(t) {
                Ok(td) => tasks.push((i, td)),
                Err(e) => {
                    return Err(Error::BadToolArgs {
                        name: "spawn_workers_parallel".into(),
                        message: format!("task[{i}]: {e}"),
                    });
                }
            }
        }

        // Pre-resolve pool roles (requires async, can't do inside join_all closures easily)
        let mut resolved_roles: Vec<Option<(String, usize, Vec<String>)>> =
            Vec::with_capacity(tasks.len());
        for (_, task) in &tasks {
            let role = if let (Some(role_name), Some(pool)) = (&task.role_name, &self.pool) {
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
            resolved_roles.push(role);
        }

        // Pre-register all workers in the registry so they can receive messages
        // from each other via send_message. Each worker gets a stable ID.
        let mut mailboxes: Vec<Option<WorkerMailbox>> = Vec::with_capacity(tasks.len());
        let mut worker_ids: Vec<String> = Vec::with_capacity(tasks.len());
        for (_, task) in &tasks {
            let wid = task
                .worker_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let mailbox = if let Some(reg) = &self.registry {
                Some(reg.register(&wid).await)
            } else {
                None
            };
            worker_ids.push(wid);
            mailboxes.push(mailbox);
        }

        // Spawn all workers concurrently
        let futures: Vec<_> = tasks
            .into_iter()
            .zip(resolved_roles)
            .zip(mailboxes)
            .map(|(((index, task), pool_role), mailbox)| {
                self.run_task(index, task, pool_role, mailbox)
            })
            .collect();

        let mut results = join_all(futures).await;

        // Deregister all workers from the registry
        if let Some(reg) = &self.registry {
            for wid in &worker_ids {
                reg.deregister(wid).await;
            }
        }

        // Sort by original index to maintain consistent output ordering
        results.sort_by_key(|(i, _)| *i);

        let output = results
            .into_iter()
            .map(|(i, text)| format!("=== Task {i} ===\n{text}"))
            .collect::<Vec<_>>()
            .join("\n\n");

        Ok(output)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Completion, MockProvider};
    use crate::tools::{LocalTransport, ReadFile, ToolTransport};
    use tempfile::TempDir;

    fn mock_provider(script: Vec<Completion>) -> Arc<dyn LlmProvider> {
        Arc::new(MockProvider::new(script))
    }

    fn make_tool(
        workspace: &std::path::Path,
        provider: Arc<dyn LlmProvider>,
    ) -> SpawnWorkersParallel {
        let transport: Arc<dyn ToolTransport> = Arc::new(LocalTransport);
        let registry = ToolRegistry::new(transport).register(Arc::new(ReadFile::new(workspace)));
        SpawnWorkersParallel::new(workspace, provider, registry, 2, 0, None)
    }

    #[tokio::test]
    async fn parallel_two_explore_workers() {
        let dir = TempDir::new().unwrap();
        let provider = mock_provider(vec![
            Completion {
                content: "Result from task A".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "Result from task B".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]);

        let tool = make_tool(dir.path(), provider);
        let result = tool
            .execute(json!({
                "tasks": [
                    {"prompt": "Explore module A", "worker_type": "explore"},
                    {"prompt": "Explore module B", "worker_type": "explore"}
                ]
            }))
            .await
            .unwrap();

        assert!(result.contains("=== Task 0 ==="), "got: {result}");
        assert!(result.contains("=== Task 1 ==="), "got: {result}");
        assert!(result.contains("type:explore"), "got: {result}");
    }

    #[tokio::test]
    async fn missing_tasks_array_errors() {
        let dir = TempDir::new().unwrap();
        let provider = mock_provider(vec![]);
        let tool = make_tool(dir.path(), provider);
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn empty_tasks_array_errors() {
        let dir = TempDir::new().unwrap();
        let provider = mock_provider(vec![]);
        let tool = make_tool(dir.path(), provider);
        let result = tool.execute(json!({"tasks": []})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn task_missing_prompt_errors() {
        let dir = TempDir::new().unwrap();
        let provider = mock_provider(vec![]);
        let tool = make_tool(dir.path(), provider);
        let result = tool
            .execute(json!({"tasks": [{"worker_type": "explore"}]}))
            .await;
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("task[0]"),
            "should mention which task failed"
        );
    }

    #[tokio::test]
    async fn depth_limit_respected() {
        let dir = TempDir::new().unwrap();
        let transport: Arc<dyn ToolTransport> = Arc::new(LocalTransport);
        let registry = ToolRegistry::new(transport);
        let provider = mock_provider(vec![]);
        // current_depth == max_depth → immediate error response (not Err)
        let tool = SpawnWorkersParallel::new(dir.path(), provider, registry, 1, 1, None);
        let result = tool
            .execute(json!({"tasks": [{"prompt": "anything"}]}))
            .await
            .unwrap();
        assert!(result.contains("depth limit"), "got: {result}");
    }
}
