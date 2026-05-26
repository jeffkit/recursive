# Goal 106 — Multi-Agent: Dynamic Team Composition

**Roadmap**: Phase 13.4 — Multi-Agent Framework (part 4/5)

**Design principle check**:
- Implemented as: team orchestrator in `src/multi.rs`
- ❌ Does NOT modify `src/agent.rs`
- Orchestrator is a higher-level pattern using AgentPool primitives

## Why

The most powerful multi-agent pattern: a "lead" agent analyzes a complex
task and dynamically decides which specialist roles to invoke, in what
order, and how to combine their results. This is the "main decides roles"
capability from the roadmap.

## Scope (do exactly this, no more)

### 1. TeamOrchestrator

```rust
/// A team orchestrator uses a lead agent to dynamically assign work
/// to specialist agents based on the task.
pub struct TeamOrchestrator {
    lead_role: String,
    available_roles: Vec<String>,
}

impl TeamOrchestrator {
    pub fn new(lead_role: String, available_roles: Vec<String>) -> Self {
        Self { lead_role, available_roles }
    }

    /// Run the orchestrator: the lead agent decides what to delegate.
    ///
    /// The lead receives the goal + a list of available specialists.
    /// Its response is parsed for delegation instructions in the format:
    /// `DELEGATE:<role>:<task description>`
    ///
    /// Each delegation is executed, results collected, then fed back
    /// to the lead for a final synthesis.
    pub async fn run(
        &self,
        pool: &AgentPool,
        goal: &str,
    ) -> Result<TeamResult, crate::Error> {
        // Phase 1: Ask the lead to plan delegations
        let delegation_prompt = format!(
            "{}\n\nAvailable specialists: {}\n\nTo delegate, use format: DELEGATE:<role>:<task>\nWhen done delegating, provide your final answer.",
            goal,
            self.available_roles.join(", ")
        );
        
        let lead_outcome = pool.run_with_role(&self.lead_role, &delegation_prompt).await?;
        
        // Parse delegation instructions from lead's response
        let lead_response = lead_outcome.transcript.iter().rev()
            .find(|m| m.role == crate::message::Role::Assistant)
            .map(|m| m.content.clone())
            .unwrap_or_default();
        
        let delegations = parse_delegations(&lead_response);
        
        // Phase 2: Execute delegations
        let mut delegation_results = Vec::new();
        for (role, task) in &delegations {
            if self.available_roles.contains(role) {
                match pool.run_with_role(role, task).await {
                    Ok(outcome) => {
                        let result = outcome.transcript.iter().rev()
                            .find(|m| m.role == crate::message::Role::Assistant)
                            .map(|m| m.content.clone())
                            .unwrap_or_default();
                        delegation_results.push(DelegationResult {
                            role: role.clone(),
                            task: task.clone(),
                            output: result,
                            success: true,
                        });
                    }
                    Err(e) => {
                        delegation_results.push(DelegationResult {
                            role: role.clone(),
                            task: task.clone(),
                            output: format!("Error: {e}"),
                            success: false,
                        });
                    }
                }
            }
        }
        
        // Phase 3: If there were delegations, feed results back to lead
        let final_output = if delegation_results.is_empty() {
            lead_response
        } else {
            let results_summary = delegation_results.iter()
                .map(|r| format!("- {} ({}): {}", r.role, if r.success { "ok" } else { "failed" }, r.output))
                .collect::<Vec<_>>()
                .join("\n");
            
            let synthesis_prompt = format!(
                "Here are the results from your delegated tasks:\n\n{}\n\nPlease provide a final synthesis.",
                results_summary
            );
            
            let synthesis = pool.run_with_role(&self.lead_role, &synthesis_prompt).await?;
            synthesis.transcript.iter().rev()
                .find(|m| m.role == crate::message::Role::Assistant)
                .map(|m| m.content.clone())
                .unwrap_or_default()
        };

        Ok(TeamResult {
            delegations: delegation_results,
            final_output,
        })
    }
}

/// Parse "DELEGATE:<role>:<task>" lines from text.
fn parse_delegations(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with("DELEGATE:") {
                let rest = &trimmed[9..];
                let parts: Vec<&str> = rest.splitn(2, ':').collect();
                if parts.len() == 2 {
                    Some((parts[0].to_string(), parts[1].to_string()))
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect()
}

#[derive(Debug)]
pub struct TeamResult {
    pub delegations: Vec<DelegationResult>,
    pub final_output: String,
}

#[derive(Debug)]
pub struct DelegationResult {
    pub role: String,
    pub task: String,
    pub output: String,
    pub success: bool,
}
```

### 2. Update lib.rs exports

Add `TeamOrchestrator`, `TeamResult`, `DelegationResult`.

### 3. Tests

- Test: parse_delegations extracts role and task
- Test: parse_delegations ignores non-delegation lines
- Test: orchestrator with no delegations returns lead's direct response
- Test: orchestrator with delegations executes them and synthesizes
- Test: delegation to unknown role is skipped (not in available_roles)

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean

## Notes for the agent

- Read `src/multi.rs` for AgentPool, run_with_role, SharedMemory, MessageBus.
- The parse_delegations function is a simple line parser — no complex parsing.
- For tests, use MockProvider with predictable responses. Set up the mock to
  return "DELEGATE:coder:write hello world" for the lead's first response.
- **DO NOT modify `src/agent.rs`.**
- **Keep the orchestration protocol simple (DELEGATE:role:task format).**
