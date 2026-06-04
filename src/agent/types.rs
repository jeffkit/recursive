//! Cross-cutting agent types shared by the kernel, runtime, and tools.
//!
//! These types were extracted from `agent.rs` during Goal 219.  They have
//! no dependency on the deprecated `Agent` / `StepEvent` types and are
//! re-exported from `crate::agent::*` for backward compatibility.
//!
//! When Goal 219 Commit 2 deletes the deprecated `Agent` path, this
//! module will be the sole owner of these four types.

use serde::{Deserialize, Serialize};

/// Decision returned by a permission hook to allow, deny, or transform a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    /// Let the tool execute with the original arguments.
    Allow,
    /// Block execution and return the reason as a tool error to the model.
    Deny(String),
    /// Replace the arguments before execution.
    Transform(serde_json::Value),
}

/// Controls whether the agent executes tools immediately or presents a plan first.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum PlanningMode {
    /// Execute tool calls immediately (current behavior).
    #[default]
    Immediate,
    /// Buffer tool calls and emit a plan for confirmation before executing.
    PlanFirst,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
/// Why the agent's run terminated.
///
/// # Variants
///
/// - `NoMoreToolCalls`: Model produced a response without tool calls (natural completion).
/// - `BudgetExceeded`: Ran out of steps (hit `max_steps`). Agent likely unfinished.
/// - `ProviderStop(reason)`: LLM provider stopped unexpectedly. `reason` may be "length"
///   (truncated by token limit), "stop"/"end_turn", or a provider-specific code.
/// - `Stuck`: Agent got stuck calling the same tool repeatedly with the same arguments.
///   `repeated_call` is the tool name, `repeats` is how many times before stopping.
/// - `TranscriptLimit`: Transcript size hit `max_transcript_chars` hard limit before
///   compaction could reduce it further. Agent cannot continue. `chars` is final size,
///   `limit` is the configured maximum.
#[non_exhaustive]
pub enum FinishReason {
    /// Model generated final response without requesting more tools.
    NoMoreToolCalls,
    /// Agent exceeded the maximum number of steps allowed.
    BudgetExceeded,
    /// LLM provider stopped with a specific reason or status code.
    ProviderStop(String),
    /// Agent detected repeated identical tool calls (stuck loop).
    Stuck {
        repeated_call: String,
        repeats: usize,
    },
    /// Transcript size exceeded hard limit and cannot be reduced further.
    TranscriptLimit { chars: usize, limit: usize },
    /// Agent proposed a plan (PlanFirst mode) and is waiting for confirmation.
    PlanPending,
    /// Agent was cancelled by a shutdown signal (SIGINT/SIGTERM).
    Cancelled,

    /// The auto permission classifier reached its denial limit
    /// (3 consecutive or 10 total denials). All subsequent tool
    /// calls are blocked to prevent denial loops.
    PermissionDenialLimit,
}

impl std::fmt::Display for FinishReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FinishReason::NoMoreToolCalls => write!(f, "no_more_tool_calls"),
            FinishReason::BudgetExceeded => write!(f, "budget_exceeded"),
            FinishReason::ProviderStop(reason) => write!(f, "provider_stop:{reason}"),
            FinishReason::Stuck {
                repeated_call,
                repeats,
            } => write!(f, "stuck:{repeated_call}:{repeats}"),
            FinishReason::TranscriptLimit { chars, limit } => {
                write!(f, "transcript_limit:{chars}/{limit}")
            }
            FinishReason::PlanPending => write!(f, "plan_pending"),
            FinishReason::Cancelled => write!(f, "cancelled"),
            FinishReason::PermissionDenialLimit => write!(f, "permission_denial_limit"),
        }
    }
}
