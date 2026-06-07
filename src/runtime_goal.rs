//! Goal-loop primitives: [`GoalStatus`], [`GoalState`], [`GoalVerdict`], and
//! the [`GoalEvaluator`] judge model.
//!
//! These types live here so [`crate::runtime`] only carries the
//! [`AgentRuntime`](crate::runtime::AgentRuntime) impl + its loop method.
//! The actual `run_goal_loop` body stays in `runtime.rs` because it mutates
//! private runtime state.
//!
//! All types are re-exported via `crate::runtime::Goal*` for backwards
//! compatibility, so external callers (e.g. `src/http.rs:496`,
//! `src/tui/app.rs`) continue to work unchanged.

use std::sync::Arc;

use crate::error::Result;
use crate::llm::LlmProvider;
use crate::message::Message;

/// Status of an active goal loop.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    /// The loop is running — condition not yet confirmed met.
    Pursuing,
    /// Condition confirmed met — goal cleared after success.
    Achieved,
}

/// Per-session goal state set by `/goal <condition>`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GoalState {
    /// The completion condition as written by the user.
    pub condition: String,
    /// Current status of the loop.
    pub status: GoalStatus,
    /// Turns elapsed since the goal was set.
    pub turns: u32,
    /// Hard cap: stop regardless of condition after this many turns.
    pub max_turns: u32,
    /// Most recent judge model verdict (reason string).
    pub last_reason: Option<String>,
}

/// Verdict returned by [`GoalEvaluator::evaluate`].
#[derive(Debug, Clone)]
pub struct GoalVerdict {
    /// Whether the condition is satisfied.
    pub achieved: bool,
    /// Judge's brief explanation.
    pub reason: String,
}

/// Calls the LLM provider to decide whether a goal condition is met.
pub struct GoalEvaluator {
    provider: Arc<dyn LlmProvider>,
}

impl GoalEvaluator {
    /// Create an evaluator backed by `provider`.
    pub fn new(provider: Arc<dyn LlmProvider>) -> Self {
        Self { provider }
    }

    /// Evaluate `condition` against the last N messages of `transcript`.
    ///
    /// Calls the provider with a minimal YES/NO prompt (max_tokens ≈ 256 via
    /// a short system instruction).  The first word of the response determines
    /// the verdict; any remaining text is kept as `reason`.
    pub async fn evaluate(&self, condition: &str, transcript: &[Message]) -> Result<GoalVerdict> {
        // Only send the last 20 messages to keep the prompt cheap.
        const TAIL: usize = 20;
        let tail = if transcript.len() > TAIL {
            &transcript[transcript.len() - TAIL..]
        } else {
            transcript
        };

        // Format the recent transcript as plain text.
        let transcript_text: String = tail
            .iter()
            .filter_map(|m| {
                if m.content.is_empty() {
                    None
                } else {
                    Some(format!("[{:?}]: {}", m.role, m.content))
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        let system_msg = Message::system(
            "You are a completion evaluator. Answer YES or NO on the first line, \
             then a single short sentence explaining why.",
        );
        let user_msg = Message::user(format!(
            "Condition: {condition}\n\nRecent transcript:\n{transcript_text}\n\n\
             Is the condition met? Answer YES or NO on the first line, then a short reason."
        ));

        let messages = vec![system_msg, user_msg];
        let completion = self.provider.complete(&messages, &[]).await?;
        let text = completion.content.trim().to_string();

        let first_line = text.lines().next().unwrap_or("").trim().to_uppercase();
        let achieved = first_line.starts_with("YES");
        let reason = text
            .lines()
            .skip(1)
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_string();
        let reason = if reason.is_empty() {
            text.clone()
        } else {
            reason
        };

        Ok(GoalVerdict { achieved, reason })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_state_serializes_and_deserializes() {
        let state = GoalState {
            condition: "all tests pass".into(),
            status: GoalStatus::Pursuing,
            turns: 3,
            max_turns: 20,
            last_reason: Some("still failing".into()),
        };
        let json = serde_json::to_string(&state).unwrap();
        let roundtrip: GoalState = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip.condition, "all tests pass");
        assert_eq!(roundtrip.status, GoalStatus::Pursuing);
        assert_eq!(roundtrip.turns, 3);
        assert_eq!(roundtrip.max_turns, 20);
        assert_eq!(roundtrip.last_reason.as_deref(), Some("still failing"));
    }

    #[test]
    fn goal_status_snake_case_serialization() {
        assert_eq!(
            serde_json::to_string(&GoalStatus::Pursuing).unwrap(),
            r#""pursuing""#
        );
        assert_eq!(
            serde_json::to_string(&GoalStatus::Achieved).unwrap(),
            r#""achieved""#
        );
    }

    #[test]
    fn goal_verdict_achieved_flag_reflects_yes_logic() {
        // Directly test the verdict construction used in evaluate().
        let text = "YES\nAll tests are passing now.";
        let first_line = text.lines().next().unwrap_or("").trim().to_uppercase();
        let achieved = first_line.starts_with("YES");
        assert!(achieved);

        let text_no = "NO\nTests still failing.";
        let first_line_no = text_no.lines().next().unwrap_or("").trim().to_uppercase();
        let achieved_no = first_line_no.starts_with("YES");
        assert!(!achieved_no);
    }
}
