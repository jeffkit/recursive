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

/// Controls how the agent executes tool calls.
///
/// Currently only `Immediate` is supported. Agent-driven planning is handled by
/// the `enter_plan_mode` / `exit_plan_mode` tool pair (Plan Mode 2.0), which does
/// not require a separate runtime mode flag.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum PlanningMode {
    /// Execute tool calls immediately.
    #[default]
    Immediate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
/// Why the agent's run terminated.
///
/// # Variants
///
/// - `NoMoreToolCalls`: Model produced a response without tool calls (natural completion).
/// - `BudgetExceeded`: Ran out of steps (when `max_steps > 0`). Agent likely unfinished.
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
            FinishReason::Cancelled => write!(f, "cancelled"),
            FinishReason::PermissionDenialLimit => write!(f, "permission_denial_limit"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_reason_display_all_variants() {
        // kills `replace <impl Display for FinishReason>::fmt with Ok(Default::default())`
        // and individual match-arm replacements.
        assert_eq!(
            FinishReason::NoMoreToolCalls.to_string(),
            "no_more_tool_calls"
        );
        assert_eq!(FinishReason::BudgetExceeded.to_string(), "budget_exceeded");
        assert_eq!(
            FinishReason::ProviderStop("length".into()).to_string(),
            "provider_stop:length"
        );
        assert_eq!(
            FinishReason::Stuck {
                repeated_call: "Read".into(),
                repeats: 3
            }
            .to_string(),
            "stuck:Read:3"
        );
        assert_eq!(
            FinishReason::TranscriptLimit {
                chars: 10_000,
                limit: 8_000
            }
            .to_string(),
            "transcript_limit:10000/8000"
        );
        assert_eq!(FinishReason::Cancelled.to_string(), "cancelled");
        assert_eq!(
            FinishReason::PermissionDenialLimit.to_string(),
            "permission_denial_limit"
        );
    }

    #[test]
    fn planning_mode_default_is_immediate() {
        assert_eq!(PlanningMode::default(), PlanningMode::Immediate);
    }

    #[test]
    fn finish_reason_struct_variants_serialize_with_kind_tag() {
        // kills mutations swapping `kind` tag values for struct/unit variants
        let json = serde_json::to_value(&FinishReason::NoMoreToolCalls).unwrap();
        assert_eq!(json["kind"], "no_more_tool_calls");

        let json = serde_json::to_value(&FinishReason::BudgetExceeded).unwrap();
        assert_eq!(json["kind"], "budget_exceeded");

        let json = serde_json::to_value(&FinishReason::Stuck {
            repeated_call: "Bash".into(),
            repeats: 5,
        })
        .unwrap();
        assert_eq!(json["kind"], "stuck");
        assert_eq!(json["repeated_call"], "Bash");
        assert_eq!(json["repeats"], 5);

        let json = serde_json::to_value(&FinishReason::TranscriptLimit {
            chars: 1000,
            limit: 500,
        })
        .unwrap();
        assert_eq!(json["kind"], "transcript_limit");
        assert_eq!(json["chars"], 1000);
        assert_eq!(json["limit"], 500);

        let json = serde_json::to_value(&FinishReason::Cancelled).unwrap();
        assert_eq!(json["kind"], "cancelled");

        let json = serde_json::to_value(&FinishReason::PermissionDenialLimit).unwrap();
        assert_eq!(json["kind"], "permission_denial_limit");
    }

    #[test]
    fn finish_reason_deserializes_from_kind_tag() {
        // kills mutations that swap field names on deserialization
        let r: FinishReason = serde_json::from_str(r#"{"kind":"no_more_tool_calls"}"#).unwrap();
        assert!(matches!(r, FinishReason::NoMoreToolCalls));

        let r: FinishReason =
            serde_json::from_str(r#"{"kind":"stuck","repeated_call":"Read","repeats":3}"#).unwrap();
        match r {
            FinishReason::Stuck {
                repeated_call,
                repeats,
            } => {
                assert_eq!(repeated_call, "Read");
                assert_eq!(repeats, 3);
            }
            other => panic!("expected Stuck, got {other:?}"),
        }

        let r: FinishReason = serde_json::from_str(r#"{"kind":"cancelled"}"#).unwrap();
        assert!(matches!(r, FinishReason::Cancelled));
    }

    #[test]
    fn permission_decision_serializes_deny_and_allow() {
        // kills variant swap mutations in PermissionDecision serialization
        let deny = serde_json::to_value(PermissionDecision::Deny("blocked".into())).unwrap();
        // snake_case rename_all → key is "deny"
        assert!(
            deny.get("deny").is_some(),
            "Deny must have 'deny' key, got {deny}"
        );

        let allow = serde_json::to_value(&PermissionDecision::Allow).unwrap();
        assert_eq!(
            allow,
            serde_json::json!("allow"),
            "Allow must serialize to string 'allow'"
        );
    }
}
