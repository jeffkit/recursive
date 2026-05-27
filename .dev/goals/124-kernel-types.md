# Goal 124 — TurnContext + TurnOutcome + AgentKernel shell

**Roadmap**: Kernel Architecture Refactor — Phase 1b (new abstractions)

**Design principle check**:
- Implemented as: new `src/kernel.rs` module
- Pure addition — no existing code modified (except lib.rs re-export)
- Does NOT modify agent.rs, main.rs, http.rs, or any existing module

## Why

The Agent Run Kernel architecture requires well-defined input/output types for
the stateless execution kernel. `TurnContext` is the prepared input (messages,
tools, config); `TurnOutcome` is the result (new messages, usage, finish
reason, side effects).

These types must exist before we can refactor Agent::run() to use them internally.

## Scope (do exactly this, no more)

### 1. Create `src/kernel.rs`

```rust
use std::sync::Arc;
use std::time::Duration;

use crate::agent::{FinishReason, PlanningMode, PermissionHook};
use crate::event::EventSink;
use crate::llm::{LlmProvider, ToolSpec, TokenUsage};
use crate::message::Message;
use crate::tools::ToolRegistry;

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
    ScheduleWakeup {
        delay: Duration,
        prompt: String,
    },
}

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

/// Builder for AgentKernel.
#[derive(Default)]
pub struct AgentKernelBuilder {
    llm: Option<Arc<dyn LlmProvider>>,
    tools: Option<ToolRegistry>,
    max_steps: Option<usize>,
}

impl AgentKernelBuilder {
    pub fn llm(mut self, llm: Arc<dyn LlmProvider>) -> Self { ... }
    pub fn tools(mut self, tools: ToolRegistry) -> Self { ... }
    pub fn max_steps(mut self, n: usize) -> Self { ... }
    pub fn build(self) -> crate::error::Result<AgentKernel> { ... }
}
```

### 2. Wire into lib.rs

Add `pub mod kernel;` and re-export:
```rust
pub use kernel::{AgentKernel, AgentKernelBuilder, TurnContext, TurnOutcome, SideEffect};
```

### 3. Tests

- `kernel_builder_requires_llm` — build without llm → error
- `kernel_builder_happy_path` — build with all fields → Ok
- `kernel_clone_is_independent` — clone kernel, modify tools on clone, original unchanged
- `kernel_with_tools_preserves_llm` — with_tools creates new kernel with same LLM
- `turn_outcome_default_values` — construct TurnOutcome, verify fields
- `side_effect_variants` — construct each SideEffect variant

## Acceptance

- `cargo test` green (505+ tests, new tests added)
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- New module `src/kernel.rs` exists with all types
- `lib.rs` re-exports the public types
- **No changes** to agent.rs, main.rs, http.rs, runner.rs, multi.rs

## Notes for the agent

- Read `src/agent.rs` lines 159-185 for `FinishReason` and `AgentOutcome` — these are the types you reference but don't duplicate.
- Read `src/llm/mod.rs` for `LlmProvider`, `ToolSpec`, `TokenUsage` types.
- Read `src/tools/mod.rs` for `ToolRegistry` — you need to know it's `Clone` (it wraps `Arc`).
- `AgentKernel::run()` is deliberately NOT implemented here. Just have the struct + builder. The actual loop extraction happens in Goal C.
- The builder pattern should mirror `AgentBuilder`'s style (chain methods, `build()` returns `Result`).
- `ToolRegistry` must be `Clone` for `AgentKernel` to be `Clone`. Verify this is the case (it is — it wraps `Arc` internally).
- **DO NOT touch any existing file except `src/lib.rs`** (adding `pub mod kernel;` and re-exports).
