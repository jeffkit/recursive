//! `stop_loop` tool — let the agent end the event-driven loop itself.
//!
//! In loop mode the agent keeps the loop alive by arming `schedule_wakeup`,
//! a `watch_file`, or a pending `run_background` job. Until now it had no way
//! to *stop* the loop — only the user could, via `/loop stop`. That made the
//! loop lifecycle opaque to the user: they had to know to type `/loop stop`.
//!
//! This tool writes a [`LoopControl::Stop`] onto the shared
//! [`BackgroundJobManager`]. The TUI loop arbiter drains it between turns and
//! exits loop mode, returning the TUI to normal interactive mode. So when the
//! supervised command reaches a terminal outcome — or the user says "stop /
//! exit the loop" in natural language — the agent calls `stop_loop` and the
//! loop ends cleanly.
//!
//! State lives on the shared background-job manager so the arbiter — which
//! already holds that manager — can read it without a new shared slot.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::run_background::{BackgroundJobManager, LoopControl};
use super::Tool;
use crate::error::Result;
use crate::llm::ToolSpec;

/// The `stop_loop` tool: agent-initiated loop shutdown.
pub struct StopLoop {
    manager: Arc<Mutex<BackgroundJobManager>>,
    _root: PathBuf,
}

impl StopLoop {
    pub fn new(root: impl Into<PathBuf>, manager: Arc<Mutex<BackgroundJobManager>>) -> Self {
        Self {
            manager,
            _root: root.into(),
        }
    }
}

#[async_trait]
impl Tool for StopLoop {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "stop_loop".into(),
            description: "Stop the event-driven loop and return the TUI to normal \
                interactive mode. Call this when the work you were looping on has \
                reached a final outcome (e.g. a supervised command finished and you \
                have reported its verdict), or when the user asks to stop / exit the \
                loop in natural language. The loop stops after the current turn. \
                Takes no arguments."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {},
            }),
        }
    }

    fn is_deferred(&self) -> bool {
        true
    }

    async fn execute(&self, _args: Value) -> Result<String> {
        let mut manager = self.manager.lock().await;
        manager.set_loop_control(LoopControl::Stop);
        Ok(json!({
            "stopping": true,
            "message": "Loop will stop after this turn. Finish your report; the TUI returns to interactive mode when this turn ends."
        })
        .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mgr() -> Arc<Mutex<BackgroundJobManager>> {
        Arc::new(Mutex::new(BackgroundJobManager::new()))
    }

    #[tokio::test]
    async fn stop_loop_sets_stop_control() {
        let tool = StopLoop::new("/tmp", mgr());
        let out = tool.execute(json!({})).await.unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["stopping"], true);
    }

    #[tokio::test]
    async fn stop_loop_control_is_consumed_once() {
        let m = mgr();
        StopLoop::new("/tmp", m.clone())
            .execute(json!({}))
            .await
            .unwrap();
        {
            let mut g = m.lock().await;
            assert_eq!(g.take_loop_control(), Some(LoopControl::Stop));
        }
        // Second take returns None (consumed).
        let mut g = m.lock().await;
        assert_eq!(g.take_loop_control(), None);
    }

    #[tokio::test]
    async fn stop_loop_is_deferred() {
        let tool = StopLoop::new("/tmp", mgr());
        assert!(tool.is_deferred());
    }
}
