# Goal 102 — Multi-Agent: Agent Pool + Role Definitions

**Roadmap**: Phase 13.1 — Multi-Agent Framework (part 1/5)

**Design principle check**:
- Implemented as: new module `src/multi.rs` (agent pool management)
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- Agent pool is a higher-level orchestration layer using Agent as a building block

## Why

Multi-agent systems need a way to define different agent roles (planner,
coder, reviewer, etc.) and manage a pool of agents that can be dispatched
to tasks. This is the foundation layer — defining what a "role" is and
how agents are pooled.

## Scope (do exactly this, no more)

### 1. `src/multi.rs` — new module

```rust
//! Multi-agent orchestration: agent pool and role definitions.

use crate::{Agent, AgentOutcome, Config, LlmProvider};
use std::collections::HashMap;
use std::sync::Arc;

/// Definition of an agent role — system prompt, tools, constraints.
#[derive(Clone, Debug)]
pub struct AgentRole {
    /// Unique name for this role (e.g., "planner", "coder", "reviewer")
    pub name: String,
    /// System prompt for agents with this role
    pub system_prompt: String,
    /// Maximum steps allowed for this role
    pub max_steps: usize,
    /// Optional: tool names this role is allowed to use (empty = all)
    pub allowed_tools: Vec<String>,
}

/// An agent pool manages multiple agents with different roles.
pub struct AgentPool {
    roles: HashMap<String, AgentRole>,
    provider: Arc<dyn LlmProvider>,
    config: Config,
}

impl AgentPool {
    /// Create a new empty agent pool.
    pub fn new(provider: Arc<dyn LlmProvider>, config: Config) -> Self {
        Self {
            roles: HashMap::new(),
            provider,
            config,
        }
    }

    /// Register a role definition.
    pub fn add_role(&mut self, role: AgentRole) {
        self.roles.insert(role.name.clone(), role);
    }

    /// Get a role by name.
    pub fn get_role(&self, name: &str) -> Option<&AgentRole> {
        self.roles.get(name)
    }

    /// List all registered role names.
    pub fn role_names(&self) -> Vec<&str> {
        self.roles.keys().map(|s| s.as_str()).collect()
    }

    /// Spawn an agent with a given role and run it with a goal.
    pub async fn run_with_role(
        &self,
        role_name: &str,
        goal: &str,
    ) -> Result<AgentOutcome, crate::Error> {
        let role = self.roles.get(role_name)
            .ok_or_else(|| crate::Error::Config(format!("unknown role: {role_name}")))?;
        
        let mut agent = Agent::builder()
            .llm(self.provider.clone())
            .system_prompt(role.system_prompt.clone())
            .max_steps(role.max_steps)
            .build()?;
        
        agent.run(goal).await
    }

    /// Number of registered roles.
    pub fn role_count(&self) -> usize {
        self.roles.len()
    }
}
```

### 2. `src/lib.rs` — add module

```rust
pub mod multi;
pub use multi::{AgentPool, AgentRole};
```

### 3. Default roles

Add a `default_roles()` function that returns a standard set:

```rust
pub fn default_roles() -> Vec<AgentRole> {
    vec![
        AgentRole {
            name: "planner".into(),
            system_prompt: "You are a planning agent. Analyze the task, break it into steps, and output a structured plan. Do not execute — only plan.".into(),
            max_steps: 10,
            allowed_tools: vec![],
        },
        AgentRole {
            name: "coder".into(),
            system_prompt: "You are a coding agent. Implement the task using the available tools. Write code, run tests, fix errors.".into(),
            max_steps: 50,
            allowed_tools: vec![],
        },
        AgentRole {
            name: "reviewer".into(),
            system_prompt: "You are a code review agent. Read the code changes, identify issues, suggest improvements. Do not modify files.".into(),
            max_steps: 20,
            allowed_tools: vec!["read_file".into(), "search_files".into()],
        },
    ]
}
```

### 4. Tests

- Test: AgentPool::new creates empty pool
- Test: add_role + get_role works
- Test: role_names returns all registered roles
- Test: run_with_role with unknown role returns error
- Test: run_with_role with mock provider succeeds
- Test: default_roles returns 3 roles with expected names

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- AgentPool can register roles and dispatch agents

## Notes for the agent

- Read `src/agent.rs` for `Agent::builder()` and `AgentOutcome`.
- Read `src/error.rs` for the `Error` enum — use `Error::Config(String)` for role not found.
- Read `src/llm/mock.rs` for MockProvider usage in tests.
- Read `src/lib.rs` for how other modules are declared and exported.
- The `allowed_tools` field is defined but not enforced yet — that's a future goal.
  Just store it in the struct.
- **DO NOT modify `src/agent.rs`.**
- **DO NOT implement inter-agent communication yet — that's g104.**
