//! Turn-level types for the Agent Run Kernel architecture.
//!
//! This module defines the input/output contract for a single turn of
//! agent execution:
//!
//! * [`TurnContext`] — everything the kernel needs to execute one turn
//!   (messages, tools, config, event sink).
//! * [`TurnOutcome`] — the result of executing one turn (new messages,
//!   usage, finish reason, side effects).
//! * [`SideEffect`] — side effects that outlive the turn (background jobs,
//!   scheduled wakeups).
//! * [`AgentKernel`] — the stateless single-turn executor (struct + builder
//!   only; `run()` is not yet implemented).
//!
//! # Design
//!
//! The Kernel is stateless and knows nothing about transcripts, sessions,
//! or cross-turn state. The Wrapper (`AgentRuntime`) prepares a
//! `TurnContext` from its transcript, calls the kernel, and then
//! incorporates the `TurnOutcome` back into its state.

use std::sync::Arc;
use std::time::Duration;

use crate::agent::{FinishReason, PermissionHook, PlanningMode};
use crate::event::EventSink;
use crate::llm::{LlmProvider, TokenUsage, ToolSpec};
use crate::message::Message;
use crate::tools::ToolRegistry;

// ---------------------------------------------------------------------------
// TurnContext
// ---------------------------------------------------------------------------

/// Everything the Kernel needs to execute one turn.
///
/// Prepared by the Wrapper (AgentRuntime). The Kernel does not know
/// where these messages came from — could be fresh, compacted, or resumed.
pub struct TurnContext {
    /// The full message list to send to the LLM (system + history + new user msg).
    pub messages: Vec<Message>,

    /// Where to emit real-time events during execution.
    pub event_sink: Box<dyn EventSink>,

    /// Tool specifications to advertise to the LLM.
    pub tool_specs: Vec<ToolSpec>,

    /// Whether to stream LLM responses token-by-token.
    pub streaming: bool,

    /// Optional permission hook for gating tool calls.
    pub permission_hook: Option<PermissionHook>,

    /// Planning mode (execute immediately vs buffer for confirmation).
    pub planning_mode: PlanningMode,
}

// ---------------------------------------------------------------------------
// TurnOutcome
// ---------------------------------------------------------------------------

/// The result of executing one turn.
///
/// Returned to the Wrapper, which appends new_messages to its transcript,
/// persists them, handles side effects, and tracks costs.
pub struct TurnOutcome {
    /// All messages produced during this turn (assistant responses + tool results).
    /// Does NOT include the input messages — only what the kernel generated.
    pub new_messages: Vec<Message>,

    /// The final assistant text (convenience — also the last assistant msg in new_messages).
    pub final_text: Option<String>,

    /// Why the turn ended.
    pub finish_reason: FinishReason,

    /// Cumulative token usage across all LLM calls in this turn.
    pub usage: TokenUsage,

    /// Total LLM call latency in milliseconds (excluding tool execution time).
    pub llm_latency_ms: u64,

    /// Number of steps (LLM invocations) executed in this turn.
    pub steps: usize,

    /// Side effects the Wrapper should adopt (background jobs, scheduled tasks).
    pub side_effects: Vec<SideEffect>,
}

// ---------------------------------------------------------------------------
// SideEffect
// ---------------------------------------------------------------------------

/// A side effect produced during a turn that outlives the turn itself.
/// The Wrapper is responsible for managing these.
#[derive(Debug, Clone)]
pub enum SideEffect {
    /// A background process was spawned (e.g. via run_background tool).
    BackgroundJob {
        id: String,
        pid: u32,
        command: String,
    },
    /// The agent requested a future wakeup (e.g. via schedule_wakeup tool).
    ScheduleWakeup { delay: Duration, prompt: String },
}

// ---------------------------------------------------------------------------
// AgentKernel
// ---------------------------------------------------------------------------

/// The stateless Agent Kernel — a single-turn ReAct executor.
///
/// Cheap to create, safe to clone, safe to share across threads.
/// Does not own transcript, session, or any cross-turn state.
///
/// NOTE: The `run()` method is NOT implemented in this goal.
/// This goal only defines the struct and its builder. The actual
/// execution logic will be wired in Goal C (Phase 2).
#[derive(Clone)]
pub struct AgentKernel {
    /// The LLM provider to use for completions.
    pub(crate) llm: Arc<dyn LlmProvider>,
    /// The tool registry (tools available to the agent).
    pub(crate) tools: ToolRegistry,
    /// Maximum number of LLM calls per turn.
    pub(crate) max_steps: usize,
}

impl AgentKernel {
    /// Create a new builder for `AgentKernel`.
    pub fn builder() -> AgentKernelBuilder {
        AgentKernelBuilder::default()
    }

    /// Access the LLM provider.
    pub fn llm(&self) -> &Arc<dyn LlmProvider> {
        &self.llm
    }

    /// Access the tool registry.
    pub fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    /// Create a new kernel with a different tool registry (same LLM, same config).
    /// Useful for Multi-Agent scenarios where sub-agents get restricted tool subsets.
    pub fn with_tools(&self, tools: ToolRegistry) -> Self {
        Self {
            llm: self.llm.clone(),
            tools,
            max_steps: self.max_steps,
        }
    }
}

// ---------------------------------------------------------------------------
// AgentKernelBuilder
// ---------------------------------------------------------------------------

/// Builder for [`AgentKernel`].
#[derive(Default)]
pub struct AgentKernelBuilder {
    llm: Option<Arc<dyn LlmProvider>>,
    tools: Option<ToolRegistry>,
    max_steps: Option<usize>,
}

impl AgentKernelBuilder {
    /// Set the LLM provider.
    pub fn llm(mut self, llm: Arc<dyn LlmProvider>) -> Self {
        self.llm = Some(llm);
        self
    }

    /// Set the tool registry.
    pub fn tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = Some(tools);
        self
    }

    /// Set the maximum number of LLM calls per turn.
    pub fn max_steps(mut self, n: usize) -> Self {
        self.max_steps = Some(n);
        self
    }

    /// Build the `AgentKernel`, or return an error if required fields are missing.
    pub fn build(self) -> crate::error::Result<AgentKernel> {
        let llm = self.llm.ok_or_else(|| crate::error::Error::Config {
            message: "llm provider is required".into(),
        })?;
        let tools = self.tools.unwrap_or_else(ToolRegistry::local);
        let max_steps = self.max_steps.unwrap_or(32);
        Ok(AgentKernel {
            llm,
            tools,
            max_steps,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::MockProvider;

    // -- Builder tests ------------------------------------------------------

    #[test]
    fn kernel_builder_requires_llm() {
        let result = AgentKernel::builder().build();
        assert!(result.is_err());
        match result {
            Err(e) => assert!(e.to_string().contains("llm provider is required")),
            Ok(_) => panic!("expected Err"),
        }
    }

    #[test]
    fn kernel_builder_happy_path() {
        let mock = MockProvider::default();
        let tools = ToolRegistry::local();
        let kernel = AgentKernel::builder()
            .llm(Arc::new(mock))
            .tools(tools)
            .max_steps(16)
            .build()
            .expect("build should succeed");
        assert_eq!(kernel.max_steps, 16);
    }

    #[test]
    fn kernel_builder_default_max_steps() {
        let mock = MockProvider::default();
        let tools = ToolRegistry::local();
        let kernel = AgentKernel::builder()
            .llm(Arc::new(mock))
            .tools(tools)
            .build()
            .expect("build should succeed");
        assert_eq!(kernel.max_steps, 32);
    }

    // -- Clone / with_tools tests ------------------------------------------

    #[test]
    fn kernel_clone_is_independent() {
        let mock = MockProvider::default();
        let tools1 = ToolRegistry::local();
        let kernel = AgentKernel::builder()
            .llm(Arc::new(mock))
            .tools(tools1)
            .build()
            .expect("build should succeed");

        let mut cloned = kernel.clone();
        // Modify the clone's tools by creating a new registry
        let new_tools = ToolRegistry::local();
        cloned.tools = new_tools;

        // The original should still have its original tools
        // (we can't compare ToolRegistry directly, but we can check
        // that the clone's tools are different by checking the transport)
        assert!(!Arc::ptr_eq(
            kernel.tools().transport(),
            cloned.tools().transport()
        ));
    }

    #[test]
    fn kernel_with_tools_preserves_llm() {
        let mock = MockProvider::default();
        let mock_arc = Arc::new(mock);
        let tools1 = ToolRegistry::local();
        let kernel = AgentKernel::builder()
            .llm(mock_arc.clone())
            .tools(tools1)
            .build()
            .expect("build should succeed");

        let tools2 = ToolRegistry::local();
        let new_kernel = kernel.with_tools(tools2);

        // LLM provider should be the same Arc
        assert!(Arc::ptr_eq(&kernel.llm, &new_kernel.llm));
        // max_steps should be preserved
        assert_eq!(new_kernel.max_steps, kernel.max_steps);
    }

    // -- TurnOutcome tests --------------------------------------------------

    #[test]
    fn turn_outcome_default_values() {
        let outcome = TurnOutcome {
            new_messages: vec![],
            final_text: None,
            finish_reason: FinishReason::NoMoreToolCalls,
            usage: TokenUsage::default(),
            llm_latency_ms: 0,
            steps: 0,
            side_effects: vec![],
        };
        assert!(outcome.new_messages.is_empty());
        assert!(outcome.final_text.is_none());
        assert_eq!(outcome.finish_reason, FinishReason::NoMoreToolCalls);
        assert_eq!(outcome.usage, TokenUsage::default());
        assert_eq!(outcome.llm_latency_ms, 0);
        assert_eq!(outcome.steps, 0);
        assert!(outcome.side_effects.is_empty());
    }

    // -- SideEffect tests ---------------------------------------------------

    #[test]
    fn side_effect_variants() {
        let bg = SideEffect::BackgroundJob {
            id: "job-1".into(),
            pid: 12345,
            command: "echo hello".into(),
        };
        match &bg {
            SideEffect::BackgroundJob { id, pid, command } => {
                assert_eq!(id, "job-1");
                assert_eq!(*pid, 12345);
                assert_eq!(command, "echo hello");
            }
            _ => panic!("expected BackgroundJob"),
        }

        let wake = SideEffect::ScheduleWakeup {
            delay: Duration::from_secs(60),
            prompt: "check status".into(),
        };
        match &wake {
            SideEffect::ScheduleWakeup { delay, prompt } => {
                assert_eq!(delay.as_secs(), 60);
                assert_eq!(prompt, "check status");
            }
            _ => panic!("expected ScheduleWakeup"),
        }
    }
}
