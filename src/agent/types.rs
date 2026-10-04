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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

    /// Wall-clock deadline exceeded (Goal 345). The agent's
    /// `--wall-timeout` was reached before any other finish
    /// condition.
    WallClockExceeded { secs: u64 },
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
            FinishReason::WallClockExceeded { secs } => {
                write!(f, "wall_clock_exceeded:{secs}")
            }
        }
    }
}

/// Inject the skill catalog as a `<system-reminder>` block appended to the
/// **tail of the system message** of the per-request message copy.
///
/// Why the system tail (and no longer a trailing `user` turn): the catalog is
/// re-sent on every step, so its *offset* inside the request decides whether
/// the provider's prefix cache can hit it. A trailing `user` turn drifts
/// forward by one history turn per step — the cacheable prefix ends at the
/// previous step's history, so the whole catalog (up to
/// `RECURSIVE_SKILL_INDEX_BUDGET` bytes, 8000 by default) is re-billed at full
/// price on every step. At the system tail the offset is fixed: the request
/// for step N is a byte-prefix of the request for step N+1, so only genuinely
/// new messages are billed.
///
/// The catalog is still NOT inlined into the assembled *static* system prompt
/// (`crate::system_prompt::assemble_system_prompt`) — it is a per-request
/// decoration, so loading/unloading a skill never rewrites the stored
/// transcript. Only the system message of the copy is rewritten; message order
/// is otherwise untouched, so AGENTS.md invariant #8 (assistant→tool_result
/// pairing) holds.
///
/// With no skills registered the transcript is returned borrowed, so the
/// no-skills path copies nothing.
pub(crate) fn inject_skill_reminder<'a>(
    messages: &'a [crate::message::Message],
    skills: &[crate::skills::Skill],
) -> std::borrow::Cow<'a, [crate::message::Message]> {
    if skills.is_empty() {
        return std::borrow::Cow::Borrowed(messages);
    }
    let reminder = crate::skills::skill_reminder(skills);
    let mut out = messages.to_vec();
    // The prompt can sit at index 0 or 1 only: `call_llm` may prepend an
    // `<available-deferred-tools>` user block. The leading-two bound also keeps
    // globs-matched skill injections — later `System` messages in the
    // transcript — from being mistaken for the system prompt.
    match out
        .iter()
        .take(2)
        .position(|m| m.role == crate::message::Role::System)
    {
        Some(idx) => {
            out[idx].content.push_str("\n\n");
            out[idx].content.push_str(&reminder);
        }
        None => out.insert(0, crate::message::Message::system(reminder)),
    }
    std::borrow::Cow::Owned(out)
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
        assert_eq!(
            FinishReason::WallClockExceeded { secs: 120 }.to_string(),
            "wall_clock_exceeded:120"
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

        let json = serde_json::to_value(&FinishReason::WallClockExceeded { secs: 300 }).unwrap();
        assert_eq!(json["kind"], "wall_clock_exceeded");
        assert_eq!(json["secs"], 300);
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

        let r: FinishReason =
            serde_json::from_str(r#"{"kind":"wall_clock_exceeded","secs":99}"#).unwrap();
        assert!(matches!(r, FinishReason::WallClockExceeded { secs: 99 }));
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

    // ── issue #95: skill reminder placement / prefix-cache stability ──────

    fn skill(name: &str, description: &str) -> crate::skills::Skill {
        crate::skills::skill_from_content(name, description, vec![])
    }

    fn serialized(messages: &[crate::message::Message]) -> Vec<String> {
        messages
            .iter()
            .map(|m| serde_json::to_string(m).expect("message serializes"))
            .collect()
    }

    /// The reminder must ride on the system message — appended to its
    /// content — so its offset inside the request never moves. A trailing
    /// `user` turn would shift it forward by one history turn per step.
    #[test]
    fn skill_reminder_is_appended_to_the_system_message() {
        let skills = vec![skill("pdf", "Manipulate PDF documents")];
        let messages = vec![
            crate::message::Message::system("SYSTEM"),
            crate::message::Message::user("hi"),
        ];

        let out = inject_skill_reminder(&messages, &skills);

        assert_eq!(out.len(), messages.len(), "no extra message is added");
        assert_eq!(out[0].role, crate::message::Role::System);
        assert!(out[0].content.starts_with("SYSTEM\n\n"), "{:?}", out[0]);
        assert!(out[0].content.contains("<system-reminder>"), "{:?}", out[0]);
        assert!(out[0].content.contains("Manipulate PDF documents"));
        assert_eq!(out[1], messages[1], "later messages are untouched");
    }

    /// The system prompt is not always index 0: `call_llm` prepends an
    /// `<available-deferred-tools>` user block when the registry has deferred
    /// tools, and globs-matched skill injections push further `System`
    /// messages. The reminder must still land on the leading system prompt.
    #[test]
    fn skill_reminder_targets_the_system_prompt_not_the_first_message() {
        let skills = vec![skill("pdf", "Manipulate PDF documents")];
        let messages = vec![
            crate::message::Message::user(
                "<available-deferred-tools>\nX\n</available-deferred-tools>",
            ),
            crate::message::Message::system("SYSTEM"),
            crate::message::Message::system("<!-- skill:glob injected -->"),
        ];

        let out = inject_skill_reminder(&messages, &skills);

        assert!(
            !out[0].content.contains("<system-reminder>"),
            "the deferred-tools block must stay clean: {:?}",
            out[0]
        );
        assert!(out[1].content.contains("<system-reminder>"), "{:?}", out[1]);
        assert_eq!(out[2], messages[2], "globs-injected system msg untouched");
    }

    /// Acceptance (issue #95): two consecutive steps of one session must send
    /// request bodies that differ only by the newly appended messages — step
    /// N's serialized message list is a byte-identical prefix of step N+1's.
    /// A tail-appended reminder breaks this on the last message of every step.
    #[test]
    fn consecutive_steps_share_a_byte_identical_request_prefix() {
        let skills = vec![skill("pdf", "Manipulate PDF documents")];

        let step1 = vec![
            crate::message::Message::system("SYSTEM"),
            crate::message::Message::user("do the thing"),
            crate::message::Message::assistant_with_tool_calls(
                "",
                vec![crate::message::ToolCall {
                    id: "call-1".into(),
                    name: "Read".into(),
                    arguments: serde_json::json!({ "path": "a.rs" }),
                }],
            ),
            crate::message::Message::tool_result("call-1", "file contents"),
        ];
        let mut step2 = step1.clone();
        step2.push(crate::message::Message::assistant("done"));
        step2.push(crate::message::Message::user("next"));

        let body1 = serialized(&inject_skill_reminder(&step1, &skills));
        let body2 = serialized(&inject_skill_reminder(&step2, &skills));

        assert_eq!(body1.len(), step1.len());
        assert!(body2.len() > body1.len());
        assert_eq!(
            body2[..body1.len()],
            body1[..],
            "step N's request prefix must be byte-identical in step N+1"
        );
    }

    /// No skills → the transcript is borrowed, not deep-copied per step.
    #[test]
    fn no_skills_borrows_the_transcript() {
        let messages = vec![crate::message::Message::system("SYSTEM")];
        let out = inject_skill_reminder(&messages, &[]);
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
    }
}
