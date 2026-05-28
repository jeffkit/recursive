//! schedule_wakeup tool — lets the agent request a timed re-invocation.
//!
//! The tool writes a `WakeupRequest` into a shared slot; `AgentRuntime::run_loop`
//! reads it after each turn completes and decides whether to sleep-then-loop.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::Tool;
use crate::error::Result;
use crate::llm::ToolSpec;

/// Shared slot where the tool writes a wakeup request.
pub type WakeupSlot = Arc<Mutex<Option<WakeupRequest>>>;

/// A wakeup request placed by the `schedule_wakeup` tool during a turn.
#[derive(Debug, Clone)]
pub struct WakeupRequest {
    pub delay: Duration,
    pub reason: String,
    pub prompt: String,
}

/// Tool that lets the agent schedule its own next invocation.
pub struct ScheduleWakeup {
    slot: WakeupSlot,
}

impl ScheduleWakeup {
    pub fn new(slot: WakeupSlot) -> Self {
        Self { slot }
    }
}

#[async_trait]
impl Tool for ScheduleWakeup {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "schedule_wakeup".into(),
            description: "Schedule the next loop iteration. The runner will \
                          sleep for delay_secs then re-invoke the agent with \
                          the given prompt. Call this to keep the loop alive."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "delay_secs": {
                        "type": "integer",
                        "description": "Seconds to sleep before next turn (1-3600)",
                        "minimum": 1,
                        "maximum": 3600
                    },
                    "reason": {
                        "type": "string",
                        "description": "Why this wakeup is needed (shown in logs)"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Goal/context for the next turn"
                    }
                },
                "required": ["delay_secs", "reason", "prompt"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let delay_secs = args["delay_secs"].as_u64().unwrap_or(60).clamp(1, 3600);
        let reason = args["reason"].as_str().unwrap_or("continue").to_string();
        let prompt = args["prompt"].as_str().unwrap_or("").to_string();

        let request = WakeupRequest {
            delay: Duration::from_secs(delay_secs),
            reason: reason.clone(),
            prompt,
        };

        if let Ok(mut slot) = self.slot.lock() {
            *slot = Some(request);
        }

        Ok(format!("Wakeup scheduled: {reason} in {delay_secs}s"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stores_wakeup_request() {
        let slot: WakeupSlot = Arc::new(Mutex::new(None));
        let tool = ScheduleWakeup::new(slot.clone());
        let args = json!({"delay_secs": 30, "reason": "check status", "prompt": "check if done"});
        let result = tool.execute(args).await.unwrap();
        assert!(result.contains("30s"));
        let req = slot.lock().unwrap().take().unwrap();
        assert_eq!(req.delay, Duration::from_secs(30));
        assert_eq!(req.prompt, "check if done");
        assert_eq!(req.reason, "check status");
    }

    #[tokio::test]
    async fn clamps_delay_max() {
        let slot: WakeupSlot = Arc::new(Mutex::new(None));
        let tool = ScheduleWakeup::new(slot.clone());
        let args = json!({"delay_secs": 9999, "reason": "x", "prompt": "y"});
        tool.execute(args).await.unwrap();
        let req = slot.lock().unwrap().take().unwrap();
        assert_eq!(req.delay, Duration::from_secs(3600));
    }

    #[tokio::test]
    async fn clamps_delay_min() {
        let slot: WakeupSlot = Arc::new(Mutex::new(None));
        let tool = ScheduleWakeup::new(slot.clone());
        let args = json!({"delay_secs": 0, "reason": "x", "prompt": "y"});
        tool.execute(args).await.unwrap();
        let req = slot.lock().unwrap().take().unwrap();
        assert_eq!(req.delay, Duration::from_secs(1));
    }

    #[tokio::test]
    async fn overwrites_previous_request() {
        let slot: WakeupSlot = Arc::new(Mutex::new(None));
        let tool = ScheduleWakeup::new(slot.clone());
        tool.execute(json!({"delay_secs": 10, "reason": "first", "prompt": "a"}))
            .await
            .unwrap();
        tool.execute(json!({"delay_secs": 20, "reason": "second", "prompt": "b"}))
            .await
            .unwrap();
        let req = slot.lock().unwrap().take().unwrap();
        assert_eq!(req.delay, Duration::from_secs(20));
        assert_eq!(req.prompt, "b");
    }
}
