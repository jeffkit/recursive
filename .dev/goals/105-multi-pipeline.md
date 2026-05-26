# Goal 105 — Multi-Agent: Pipeline Mode (A → B → C)

**Roadmap**: Phase 13.5 — Multi-Agent Framework (part 5/5)

**Design principle check**:
- Implemented as: pipeline executor in `src/multi.rs`
- ❌ Does NOT modify `src/agent.rs`
- Pipeline is orchestration on top of AgentPool

## Why

The simplest multi-agent pattern: chain agents in sequence. The output
of agent A becomes the input of agent B, then B's output feeds C.
Example: planner → coder → reviewer.

## Scope (do exactly this, no more)

### 1. Pipeline struct

```rust
/// A pipeline chains multiple agent roles in sequence.
/// Each agent's output becomes the next agent's input.
pub struct Pipeline {
    stages: Vec<String>,  // role names in order
}

impl Pipeline {
    pub fn new(stages: Vec<String>) -> Self {
        Self { stages }
    }

    /// Execute the pipeline: run each stage in sequence,
    /// passing the previous agent's final message as the next goal.
    pub async fn execute(
        &self,
        pool: &AgentPool,
        initial_goal: &str,
    ) -> Result<PipelineResult, crate::Error> {
        let mut current_input = initial_goal.to_string();
        let mut stage_outcomes = Vec::new();

        for role_name in &self.stages {
            let outcome = pool.run_with_role(role_name, &current_input).await?;
            
            // Extract the last assistant message as output for next stage
            let output = outcome.transcript.iter().rev()
                .find(|m| m.role == crate::message::Role::Assistant)
                .map(|m| m.content.clone())
                .unwrap_or_default();
            
            stage_outcomes.push(StageOutcome {
                role: role_name.clone(),
                output: output.clone(),
                steps: outcome.steps,
            });
            
            current_input = output;
        }

        Ok(PipelineResult { stages: stage_outcomes })
    }
}

#[derive(Debug)]
pub struct PipelineResult {
    pub stages: Vec<StageOutcome>,
}

#[derive(Debug)]
pub struct StageOutcome {
    pub role: String,
    pub output: String,
    pub steps: usize,
}

impl PipelineResult {
    /// Get the final output (last stage's output).
    pub fn final_output(&self) -> &str {
        self.stages.last()
            .map(|s| s.output.as_str())
            .unwrap_or("")
    }

    /// Number of stages executed.
    pub fn stage_count(&self) -> usize {
        self.stages.len()
    }
}
```

### 2. Update lib.rs exports

Add `Pipeline`, `PipelineResult`, `StageOutcome`.

### 3. Tests

- Test: empty pipeline returns empty result
- Test: single-stage pipeline runs one agent
- Test: multi-stage pipeline passes output between stages
- Test: pipeline fails if role doesn't exist
- Test: final_output returns last stage output

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean

## Notes for the agent

- Read `src/multi.rs` for AgentPool::run_with_role.
- Read `src/message.rs` for Message struct and Role enum.
- For tests, use MockProvider that returns a predictable response.
- The key logic: extract last assistant message from outcome.transcript
  as the input for the next stage.
- **DO NOT modify `src/agent.rs`.**
