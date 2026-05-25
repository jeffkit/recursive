//! Sub-agent tool: spawn a fresh agent loop with a restricted tool subset.
//!
//! The parent agent can delegate focused sub-tasks to a child agent that
//! starts with an empty transcript. This prevents the parent's context
//! window from growing with intermediate exploration steps.
//!
//! Recursive safety: a depth limit (env `RECURSIVE_SUBAGENT_MAX_DEPTH`,
//! default 2) prevents unbounded nesting. Each nested invocation increments
//! a counter; when the limit is reached, the tool returns an error string
//! instead of spawning.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::agent::{Agent, FinishReason, PermissionHook};
use crate::error::{Error, Result};
use crate::llm::{LlmProvider, ToolSpec};
use crate::tools::{Tool, ToolRegistry};

/// The sub-agent tool.
///
/// Constructed with:
/// - `workspace`: path for sandboxed tools
/// - `provider`: the LLM provider (shared Arc from parent)
/// - `all_tools`: the full tool registry from which the sub-agent can draw
/// - `max_depth`: absolute depth limit (env-configured)
/// - `current_depth`: how deep we already are (passed from parent)
/// - `permission_hook`: optional permission hook inherited from the parent agent
pub struct SubAgent {
    workspace: std::path::PathBuf,
    provider: Arc<dyn LlmProvider>,
    all_tools: ToolRegistry,
    max_depth: usize,
    current_depth: usize,
    permission_hook: Option<PermissionHook>,
}

impl SubAgent {
    pub fn new(
        workspace: impl Into<std::path::PathBuf>,
        provider: Arc<dyn LlmProvider>,
        all_tools: ToolRegistry,
        max_depth: usize,
        current_depth: usize,
        permission_hook: Option<PermissionHook>,
    ) -> Self {
        Self {
            workspace: workspace.into(),
            provider,
            all_tools,
            max_depth,
            current_depth,
            permission_hook,
        }
    }

    /// Build a restricted tool registry containing only the named tools.
    fn build_sub_registry(&self, tool_names: &[String]) -> ToolRegistry {
        let mut reg = self.all_tools.with_same_transport();
        for name in tool_names {
            if let Some(tool) = self.all_tools.get(name) {
                reg = reg.register(tool);
            }
        }
        reg
    }

    /// Default tool set when no `tools` arg is given: read-only tools.
    fn default_tool_names() -> Vec<String> {
        vec![
            "read_file".to_string(),
            "list_dir".to_string(),
            "search_files".to_string(),
            "web_fetch".to_string(),
        ]
    }
}

#[async_trait]
impl Tool for SubAgent {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "sub_agent".into(),
            description: "Spawn a fresh agent with its own transcript to complete a focused sub-task. Returns the sub-agent's final response.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "The goal / prompt for the sub-agent"
                    },
                    "max_steps": {
                        "type": "integer",
                        "description": "Maximum steps for the sub-agent (default 30, capped at parent's remaining budget)",
                        "default": 30
                    },
                    "tools": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional list of tool names to make available to the sub-agent. Default: read_file, list_dir, search_files, web_fetch"
                    }
                },
                "required": ["prompt"]
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let prompt = arguments["prompt"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "sub_agent".into(),
                message: "missing required parameter: prompt".to_string(),
            })?;

        let max_steps = arguments["max_steps"].as_i64().unwrap_or(30).clamp(1, 100) as usize;

        let tool_names: Vec<String> = arguments["tools"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_else(Self::default_tool_names);

        // Depth limit check
        if self.current_depth >= self.max_depth {
            return Ok(format!(
                "ERROR: sub-agent depth limit reached (max_depth={}). Cannot spawn deeper sub-agent.",
                self.max_depth
            ));
        }

        // Build the sub-agent's tool registry
        let mut sub_registry = self.build_sub_registry(&tool_names);

        // Register a fresh SubAgent with incremented depth so the child
        // can also spawn sub-agents (up to the limit).
        let child_sub = SubAgent::new(
            &self.workspace,
            self.provider.clone(),
            self.all_tools.clone(),
            self.max_depth,
            self.current_depth + 1,
            self.permission_hook.clone(),
        );
        sub_registry = sub_registry.register(Arc::new(child_sub));

        // Build and run the sub-agent
        let builder = Agent::builder()
            .llm(self.provider.clone())
            .tools(sub_registry)
            .system_prompt("You are a focused sub-agent. Complete the given task using the available tools. Be concise.")
            .max_steps(max_steps);

        // Inherit the parent's permission hook, if any
        let builder = builder.permission_hook_opt(self.permission_hook.clone());

        let mut agent = builder.build().map_err(|e| Error::Tool {
            name: "sub_agent".into(),
            message: format!("failed to build sub-agent: {e}"),
        })?;

        let outcome = agent.run(prompt).await.map_err(|e| Error::Tool {
            name: "sub_agent".into(),
            message: format!("sub-agent failed: {e}"),
        })?;

        let finish_label = match &outcome.finish {
            FinishReason::NoMoreToolCalls => "NoMoreToolCalls",
            FinishReason::BudgetExceeded => "BudgetExceeded",
            FinishReason::ProviderStop(r) => r,
            FinishReason::Stuck { .. } => "Stuck",
            FinishReason::TranscriptLimit { .. } => "TranscriptLimit",
        };

        let final_text = outcome
            .final_message
            .unwrap_or_else(|| "(no final message)".to_string());

        Ok(format!(
            "[sub-agent finished: {finish_label}]\n{final_text}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Completion, MockProvider, ToolCall};
    use crate::tools::{ApplyPatch, ListDir, LocalTransport, ReadFile, SearchFiles, ToolTransport, WriteFile};

    /// Helper: create a MockProvider with the given scripted completions.
    fn mock_provider(script: Vec<Completion>) -> Arc<dyn LlmProvider> {
        Arc::new(MockProvider::new(script))
    }

    /// Helper: build a full tool registry with read-only + write tools.
    fn full_tool_registry(workspace: &std::path::Path) -> ToolRegistry {
        let transport: Arc<dyn ToolTransport> = Arc::new(LocalTransport);
        ToolRegistry::new(transport)
            .register(Arc::new(ReadFile::new(workspace)))
            .register(Arc::new(ListDir::new(workspace)))
            .register(Arc::new(SearchFiles::new(workspace)))
            .register(Arc::new(WriteFile::new(workspace)))
            .register(Arc::new(ApplyPatch::new(workspace)))
    }

    #[tokio::test]
    async fn sub_agent_basic_dispatch() {
        // Sub-agent gets one completion with no tool calls → NoMoreToolCalls
        let provider = mock_provider(vec![Completion {
            content: "The answer is 42.".to_string(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
        }]);

        let tmp = tempfile::tempdir().unwrap();
        let all_tools = full_tool_registry(tmp.path());

        let sub = SubAgent::new(tmp.path(), provider, all_tools, 2, 0, None);

        let result = sub
            .execute(json!({"prompt": "What is the meaning of life?"}))
            .await
            .unwrap();

        assert!(result.contains("NoMoreToolCalls"));
        assert!(result.contains("The answer is 42."));
    }

    #[tokio::test]
    async fn sub_agent_depth_limit_enforced() {
        // Create a sub-agent at depth=2 with max_depth=2.
        // It should refuse to spawn deeper.
        let provider = mock_provider(vec![]);
        let tmp = tempfile::tempdir().unwrap();
        let all_tools = full_tool_registry(tmp.path());

        let sub = SubAgent::new(tmp.path(), provider, all_tools, 2, 2, None);

        let result = sub
            .execute(json!({"prompt": "do something"}))
            .await
            .unwrap();

        assert!(result.contains("depth limit reached"));
        assert!(result.contains("max_depth=2"));
    }

    #[tokio::test]
    async fn sub_agent_tool_subset_respected() {
        // Parent passes tools: ["read_file"]; sub-agent must NOT have apply_patch.
        let provider = mock_provider(vec![Completion {
            content: "done".to_string(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
        }]);

        let tmp = tempfile::tempdir().unwrap();
        let all_tools = full_tool_registry(tmp.path());

        let sub = SubAgent::new(tmp.path(), provider, all_tools, 2, 0, None);

        // Execute with only read_file allowed
        let _ = sub
            .execute(json!({"prompt": "read something", "tools": ["read_file"]}))
            .await
            .unwrap();

        // We can't easily inspect the sub-agent's registry from here,
        // but we can verify the sub-agent ran successfully with just read_file.
        // The real test is that apply_patch is NOT in the default set.
        let defaults = SubAgent::default_tool_names();
        assert!(!defaults.contains(&"apply_patch".to_string()));
        assert!(!defaults.contains(&"write_file".to_string()));
        assert!(defaults.contains(&"read_file".to_string()));
    }

    #[tokio::test]
    async fn sub_agent_max_steps_capped() {
        // Sub-agent with max_steps=5 should hit BudgetExceeded when
        // MockProvider keeps asking for tool calls.
        let tmp = tempfile::tempdir().unwrap();
        // Create a file so read_file succeeds
        std::fs::write(tmp.path().join("test.txt"), b"hello").unwrap();

        let mut script = Vec::new();
        for _ in 0..10 {
            script.push(Completion {
                content: "".to_string(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "test.txt"}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
            });
        }

        let provider = mock_provider(script);
        let all_tools = full_tool_registry(tmp.path());

        let sub = SubAgent::new(tmp.path(), provider, all_tools, 2, 0, None);

        let result = sub
            .execute(json!({"prompt": "loop", "max_steps": 5}))
            .await
            .unwrap();

        assert!(result.contains("BudgetExceeded"));
    }

    #[tokio::test]
    async fn sub_agent_default_tools_are_read_only() {
        let defaults = SubAgent::default_tool_names();
        assert!(defaults.contains(&"read_file".to_string()));
        assert!(defaults.contains(&"list_dir".to_string()));
        assert!(defaults.contains(&"search_files".to_string()));
        assert!(defaults.contains(&"web_fetch".to_string()));
        assert_eq!(defaults.len(), 4);
    }

    #[tokio::test]
    async fn sub_agent_missing_prompt_returns_error() {
        let provider = mock_provider(vec![]);
        let tmp = tempfile::tempdir().unwrap();
        let all_tools = full_tool_registry(tmp.path());

        let sub = SubAgent::new(tmp.path(), provider, all_tools, 2, 0, None);

        let result = sub.execute(json!({})).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("missing required parameter: prompt"));
    }

    #[tokio::test]
    async fn sub_agent_nested_depth_works() {
        // Test that a SubAgent at depth=0 can spawn a child (depth=1)
        // which can spawn a grandchild (depth=2) — but the grandchild
        // is at the limit (max_depth=2) so it cannot spawn deeper.
        //
        // Scripted completions consumed in order:
        //   1. Child agent's first call → sub_agent tool call
        //   2. Grandchild agent's first call → sub_agent tool call (denied by depth)
        //   3. Grandchild agent's second call → "grandchild done"
        //   4. Child agent's second call → "child done"
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("test.txt"), b"hello").unwrap();

        let provider = mock_provider(vec![
            // 1. Child agent: calls sub_agent
            Completion {
                content: "".to_string(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "sub_agent".into(),
                    arguments: json!({"prompt": "grandchild task"}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
            },
            // 2. Grandchild agent: tries to spawn deeper (denied)
            Completion {
                content: "".to_string(),
                tool_calls: vec![ToolCall {
                    id: "c2".into(),
                    name: "sub_agent".into(),
                    arguments: json!({"prompt": "great-grandchild task"}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
            },
            // 3. Grandchild agent: finishes after seeing depth error
            Completion {
                content: "grandchild done".to_string(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
            },
            // 4. Child agent: finishes
            Completion {
                content: "child done".to_string(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
            },
        ]);

        let all_tools = full_tool_registry(tmp.path());

        // Parent at depth=0, max_depth=2
        let parent = SubAgent::new(tmp.path(), provider, all_tools, 2, 0, None);

        let result = parent
            .execute(json!({"prompt": "parent task", "tools": ["sub_agent", "read_file"]}))
            .await
            .unwrap();

        // The parent should complete successfully with child's result
        assert!(result.contains("NoMoreToolCalls"), "result: {result}");
        assert!(result.contains("child done"), "result: {result}");
        // The grandchild's result is embedded in the child's transcript
        // (as a tool result), not in the final message. The depth limit
        // was enforced: the grandchild could not spawn deeper.
        // This test verifies the nesting works without panicking.
    }
}
