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
use crate::llm::ChatProvider;
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
    provider: Arc<dyn ChatProvider>,
}

impl GoalEvaluator {
    /// Create an evaluator backed by `provider`.
    pub fn new(provider: Arc<dyn ChatProvider>) -> Self {
        Self { provider }
    }

    /// Evaluate `condition` against the given `transcript`.
    ///
    /// Callers are responsible for pre-slicing `transcript` to the desired
    /// number of recent messages (e.g. via
    /// [`AgentRuntime::transcript_tail`]).  This method uses the full slice
    /// as-is — it does NOT perform any further truncation.
    ///
    /// Calls the provider with a minimal YES/NO prompt (max_tokens ≈ 256 via
    /// a short system instruction).  The first word of the response determines
    /// the verdict; any remaining text is kept as `reason`.
    pub async fn evaluate(&self, condition: &str, transcript: &[Message]) -> Result<GoalVerdict> {
        // Format the recent transcript as plain text.
        let transcript_text: String = transcript
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
    use std::sync::Mutex;

    use crate::error::Result;
    use crate::llm::{ChatProvider, Completion};
    use crate::message::Role;
    use async_trait::async_trait;

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

    /// Mock provider that captures the user prompt for inspection.
    struct CapturingProvider {
        captured_user_content: Mutex<String>,
    }

    #[async_trait]
    impl ChatProvider for CapturingProvider {
        async fn complete(
            &self,
            messages: &[Message],
            _tools: &[crate::llm::ToolSpec],
        ) -> Result<Completion> {
            // Capture the last (user) message content.
            if let Some(last) = messages.last() {
                *self.captured_user_content.lock().unwrap() = last.content.clone();
            }
            Ok(Completion {
                content: "YES\nAll conditions met.".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            })
        }
    }

    /// Provider that returns a fixed "NO" response, used to confirm
    /// `evaluate` can return `achieved = false`.
    struct NoProvider;
    #[async_trait]
    impl ChatProvider for NoProvider {
        async fn complete(
            &self,
            _messages: &[Message],
            _tools: &[crate::llm::ToolSpec],
        ) -> Result<Completion> {
            Ok(Completion {
                content: "NO\nCondition not yet met.".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            })
        }
    }

    #[tokio::test]
    async fn evaluate_returns_not_achieved_for_no_response() {
        // kills `starts_with("YES")` → `starts_with("NO")` mutation
        let evaluator = GoalEvaluator::new(Arc::new(NoProvider));
        let verdict = evaluator
            .evaluate("all tests pass", &[Message::user("still failing")])
            .await
            .unwrap();
        assert!(!verdict.achieved, "NO response must produce achieved=false");
        assert!(
            !verdict.reason.is_empty(),
            "reason must be non-empty even for NO"
        );
    }

    #[tokio::test]
    async fn evaluate_skips_empty_content_messages() {
        // kills `if m.content.is_empty() { None }` guard removal mutation
        let provider = Arc::new(CapturingProvider {
            captured_user_content: Mutex::new(String::new()),
        });
        let evaluator = GoalEvaluator::new(provider.clone());
        let transcript = vec![
            Message::user("visible"),
            Message::user(""), // empty — must be skipped
        ];
        let _ = evaluator.evaluate("cond", &transcript).await.unwrap();
        let captured = provider.captured_user_content.lock().unwrap().clone();
        assert!(
            captured.contains("visible"),
            "non-empty message must appear in prompt"
        );
        // Count occurrences of [User]: in the prompt; only the "visible"
        // message should appear, not the empty one.
        let occurrences = captured.matches("[User]:").count();
        assert_eq!(
            occurrences, 1,
            "only 1 non-empty User message should be included; got {occurrences} occurrences in: {captured}"
        );
    }

    /// Provider returning a bare one-liner (no second line) so `reason` falls
    /// back to the full response text.
    struct OneLinerProvider;
    #[async_trait]
    impl ChatProvider for OneLinerProvider {
        async fn complete(
            &self,
            _messages: &[Message],
            _tools: &[crate::llm::ToolSpec],
        ) -> Result<Completion> {
            Ok(Completion {
                content: "YES".into(), // single line — no reason line
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            })
        }
    }

    #[tokio::test]
    async fn evaluate_reason_falls_back_to_full_text_when_single_line() {
        // kills the `if reason.is_empty() { text.clone() }` → `""` mutation
        let evaluator = GoalEvaluator::new(Arc::new(OneLinerProvider));
        let verdict = evaluator
            .evaluate("cond", &[Message::user("ok")])
            .await
            .unwrap();
        assert!(verdict.achieved, "single-line YES must be achieved=true");
        // reason must not be empty — it falls back to the full "YES" text
        assert!(!verdict.reason.is_empty(), "reason must fall back to the full response");
        assert_eq!(verdict.reason, "YES", "reason should equal the full response when there is no second line");
    }

    /// Goal-301: a transcript with 25 messages must NOT be further
    /// truncated inside `evaluate()`.  All 25 messages must contribute
    /// to the user prompt sent to the provider.
    #[tokio::test]
    async fn evaluate_preserves_all_messages_beyond_old_tail_20() {
        // Build 25 distinct user messages.
        let transcript: Vec<Message> = (0..25)
            .map(|i| Message {
                role: Role::User,
                content: format!("message-{i:02}"),
                tool_calls: vec![],
                tool_call_id: None,
                reasoning_content: None,
                is_compaction_summary: false,
            })
            .collect();

        let provider = Arc::new(CapturingProvider {
            captured_user_content: Mutex::new(String::new()),
        });
        let evaluator = GoalEvaluator::new(provider.clone());

        let verdict = evaluator
            .evaluate("test condition", &transcript)
            .await
            .unwrap();
        assert!(verdict.achieved);

        let captured = provider.captured_user_content.lock().unwrap().clone();
        // Each message should appear with its formatted content.
        for i in 0..25 {
            let needle = format!("message-{i:02}");
            assert!(
                captured.contains(&needle),
                "transcript message {i} ({needle}) missing from prompt;\n\
                 prompt was: {captured}"
            );
        }
    }
}
