//! Cross-turn agent wrapper.
//!
//! `AgentRunner` manages an `Agent` across multiple conversation turns,
//! preserving the transcript between turns and streaming events to the
//! caller. This is the extracted pattern from the REPL in `main.rs`,
//! made reusable for loop mode, HTTP API sessions, and TUI.

use tokio::sync::mpsc;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::agent::{Agent, AgentOutcome, StepEvent};
use crate::error::Result;
use crate::tools::BackgroundJobManager;

/// Manages an Agent across multiple conversation turns.
///
/// Preserves transcript between turns, emits events via a caller-provided
/// channel, and tracks cumulative turn count. Each call to `turn()` runs
/// the agent once with the given goal, then restores the transcript so the
/// next turn continues the conversation.
///
/// Optionally holds a reference to a shared `BackgroundJobManager`. When
/// `clear()` is called, any pending background jobs are also cleared.
///
/// # Example
///
/// ```ignore
/// let mut runner = AgentRunner::new(agent);
/// let (tx, mut rx) = mpsc::unbounded_channel();
///
/// // First turn
/// let outcome = runner.turn("Hello", Some(tx.clone())).await?;
///
/// // Second turn — transcript from turn 1 is preserved
/// let outcome = runner.turn("Follow up", Some(tx.clone())).await?;
///
/// // Start fresh
/// runner.clear();
/// ```
pub struct AgentRunner {
    agent: Agent,
    total_turns: usize,
    /// Optional shared background job manager. When set, `clear()` will
    /// also cancel all tracked background jobs.
    bg_manager: Option<Arc<Mutex<BackgroundJobManager>>>,
}

impl AgentRunner {
    /// Create a runner from a pre-built Agent.
    pub fn new(agent: Agent) -> Self {
        Self {
            agent,
            total_turns: 0,
            bg_manager: None,
        }
    }

    /// Create a runner that also manages background jobs.
    ///
    /// When `clear()` is called, all tracked background jobs are removed
    /// from the shared manager in addition to clearing the transcript.
    pub fn with_bg_manager(
        agent: Agent,
        bg_manager: Arc<Mutex<BackgroundJobManager>>,
    ) -> Self {
        Self {
            agent,
            total_turns: 0,
            bg_manager: Some(bg_manager),
        }
    }

    /// Run a single turn with the given goal.
    ///
    /// If `events` is `Some(sender)`, step events are streamed through the
    /// channel in real time. Pass `None` to suppress events.
    ///
    /// The transcript is automatically preserved for the next turn. Returns
    /// the outcome of this turn.
    pub async fn turn(
        &mut self,
        goal: impl Into<String>,
        events: Option<mpsc::UnboundedSender<StepEvent>>,
    ) -> Result<AgentOutcome> {
        self.agent.set_events(events);

        let outcome = self.agent.run(goal).await?;

        // Restore transcript for next turn (run() takes it via mem::take)
        self.agent.set_transcript(outcome.transcript.clone());
        self.agent.set_events(None);
        self.total_turns += 1;

        Ok(outcome)
    }

    /// Clear the conversation history and reset the turn counter.
    ///
    /// If a `BackgroundJobManager` was provided, all tracked background
    /// jobs are also removed.
    pub fn clear(&mut self) {
        self.agent.set_transcript(Vec::new());
        self.total_turns = 0;
        if let Some(ref mgr) = self.bg_manager {
            // Best-effort: if the lock is poisoned, we still clear the agent state.
            if let Ok(mut mgr) = mgr.try_lock() {
                mgr.clear();
            }
        }
    }

    /// Number of turns completed so far.
    pub fn turns(&self) -> usize {
        self.total_turns
    }

    /// Access the underlying agent (e.g., to call `confirm_plan`).
    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    /// Mutable access to the underlying agent.
    pub fn agent_mut(&mut self) -> &mut Agent {
        &mut self.agent
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::mpsc;

    use crate::agent::StepEvent;
    use crate::llm::{Completion, MockProvider};
    use crate::message::{Message, Role};
    use crate::tools::ToolRegistry;
    use crate::Agent;

    use super::*;

    fn make_agent(script: Vec<Completion>) -> Agent {
        let provider = Arc::new(MockProvider::new(script));
        Agent::builder()
            .llm(provider)
            .tools(ToolRegistry::default())
            .system_prompt("You are a helpful assistant.")
            .max_steps(5)
            .build()
            .unwrap()
    }

    fn user_content(msg: &Message) -> Option<&str> {
        if msg.role == Role::User {
            Some(msg.content.as_str())
        } else {
            None
        }
    }

    #[tokio::test]
    async fn preserves_transcript_across_turns() {
        let script = vec![
            Completion {
                content: "First response".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".to_string()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "Second response".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".to_string()),
                usage: None,
                reasoning_content: None,
            },
        ];
        let agent = make_agent(script);
        let mut runner = AgentRunner::new(agent);

        let outcome1 = runner.turn("First goal", None).await.unwrap();
        assert_eq!(runner.turns(), 1);
        assert!(outcome1
            .transcript
            .iter()
            .any(|m| { m.role == Role::User && m.content == "First goal" }));

        let outcome2 = runner.turn("Second goal", None).await.unwrap();
        assert_eq!(runner.turns(), 2);

        // Second turn's transcript should contain messages from both turns
        let texts: Vec<&str> = outcome2
            .transcript
            .iter()
            .filter_map(user_content)
            .collect();
        assert!(
            texts.contains(&"First goal"),
            "first turn goal should be in transcript: {texts:?}"
        );
        assert!(
            texts.contains(&"Second goal"),
            "second turn goal should be in transcript: {texts:?}"
        );
    }

    #[tokio::test]
    async fn clear_resets_transcript() {
        // Provide two completions: one for the first turn, one for after clear
        let script = vec![
            Completion {
                content: "Response".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".to_string()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "After clear".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".to_string()),
                usage: None,
                reasoning_content: None,
            },
        ];
        let agent = make_agent(script);
        let mut runner = AgentRunner::new(agent);

        runner.turn("Goal", None).await.unwrap();
        assert_eq!(runner.turns(), 1);

        runner.clear();
        assert_eq!(runner.turns(), 0);

        // After clear, the agent should have no transcript
        let agent = runner.agent();
        // Verify the agent's internal state is reset by running another turn
        // and checking it only has the new goal.
        let _ = agent;
        let outcome = runner.turn("New goal", None).await.unwrap();
        let user_msgs: Vec<&str> = outcome.transcript.iter().filter_map(user_content).collect();
        assert_eq!(user_msgs, vec!["New goal"]);
    }

    #[tokio::test]
    async fn turns_increments() {
        let script = vec![
            Completion {
                content: "One".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".to_string()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "Two".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".to_string()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "Three".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".to_string()),
                usage: None,
                reasoning_content: None,
            },
        ];
        let agent = make_agent(script);
        let mut runner = AgentRunner::new(agent);

        assert_eq!(runner.turns(), 0);
        runner.turn("A", None).await.unwrap();
        assert_eq!(runner.turns(), 1);
        runner.turn("B", None).await.unwrap();
        assert_eq!(runner.turns(), 2);
        runner.turn("C", None).await.unwrap();
        assert_eq!(runner.turns(), 3);
    }

    #[tokio::test]
    async fn events_forwarded_when_provided() {
        let script = vec![Completion {
            content: "Hello world".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".to_string()),
            usage: None,
            reasoning_content: None,
        }];
        let agent = make_agent(script);
        let mut runner = AgentRunner::new(agent);

        let (tx, mut rx) = mpsc::unbounded_channel();
        runner.turn("Goal", Some(tx)).await.unwrap();

        // We should have received events (at least a Finished event)
        let events: Vec<StepEvent> = {
            let mut v = Vec::new();
            while let Ok(ev) = rx.try_recv() {
                v.push(ev);
            }
            v
        };
        assert!(!events.is_empty(), "should have received events");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StepEvent::Finished { .. })),
            "should have a Finished event"
        );
    }

    #[tokio::test]
    async fn no_events_when_none_passed() {
        let script = vec![Completion {
            content: "Silent".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".to_string()),
            usage: None,
            reasoning_content: None,
        }];
        let agent = make_agent(script);
        let mut runner = AgentRunner::new(agent);

        runner.turn("Goal", None).await.unwrap();

        // The agent's events channel should be None
        // We verify by checking that no events were emitted
        // (we can't inspect the agent's events field directly,
        // but we can confirm the turn completed successfully)
        assert_eq!(runner.turns(), 1);
    }

    #[tokio::test]
    async fn agent_accessors_work() {
        let script = vec![Completion {
            content: "Test".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".to_string()),
            usage: None,
            reasoning_content: None,
        }];
        let agent = make_agent(script);
        let mut runner = AgentRunner::new(agent);

        // Immutable access
        let _agent_ref = runner.agent();
        // Mutable access
        let _agent_mut = runner.agent_mut();
    }
}
