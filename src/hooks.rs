//! Lifecycle hooks for the agent loop.
//!
//! Hooks are callbacks invoked at well-defined points during an agent run.
//! They allow consumers to observe, log, gate, or transform behaviour without
//! modifying the agent loop itself.
//!
//! # Hook points
//!
//! - `SessionStart` — at the top of `Agent::run()`, before any LLM call.
//! - `PreToolCall` — before each tool dispatch (after the permission hook).
//! - `PostToolCall` — after each tool returns.
//! - `PreCompact` — before compaction fires.
//! - `PostCompact` — after compaction completes.
//! - `SessionEnd` — just before returning from `Agent::run()`.
//!
//! # Usage
//!
//! ```ignore
//! use recursive::hooks::{Hook, HookEvent, HookAction, HookRegistry};
//!
//! struct MyHook;
//! impl Hook for MyHook {
//!     fn on_event(&self, event: HookEvent) -> HookAction {
//!         match event {
//!             HookEvent::PreToolCall { name, .. } => {
//!                 eprintln!("about to call {name}");
//!                 HookAction::Continue
//!             }
//!             _ => HookAction::Continue,
//!         }
//!     }
//! }
//!
//! let mut registry = HookRegistry::new();
//! registry.register(Arc::new(MyHook));
//! ```

use std::sync::Arc;

use serde_json::Value;

use crate::agent::AgentOutcome;
use std::collections::HashMap;
use std::time::Instant;

/// Action a hook can request in response to an event.
#[derive(Debug, Clone)]
pub enum HookAction {
    /// Proceed normally.
    Continue,
    /// Skip this tool call (PreToolCall only; treated as Continue for other events).
    Skip,
    /// Abort with an error message (PreToolCall only; treated as Continue for other events).
    Error(String),
}

/// Events emitted at lifecycle points during an agent run.
///
/// This enum is `#[non_exhaustive]` — new variants may be added in future
/// releases without a breaking change.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum HookEvent<'a> {
    /// Fired at the start of `Agent::run()`, before any LLM call.
    SessionStart {
        /// The goal text passed to `Agent::run()`.
        goal: &'a str,
    },
    /// Fired before a tool is dispatched (after the permission hook).
    PreToolCall {
        /// Name of the tool about to be called.
        name: &'a str,
        /// Arguments that will be passed to the tool.
        args: &'a Value,
    },
    /// Fired after a tool returns.
    PostToolCall {
        /// Name of the tool that was called.
        name: &'a str,
        /// Arguments that were passed to the tool.
        args: &'a Value,
        /// The result string returned by the tool (or error message).
        result: &'a str,
        /// Wall-clock duration of the tool execution in milliseconds.
        duration_ms: u64,
    },
    /// Fired before compaction is attempted.
    PreCompact {
        /// Current transcript length in characters.
        transcript_len: usize,
    },
    /// Fired after compaction completes.
    PostCompact {
        /// Number of messages removed during compaction.
        removed: usize,
        /// Character count of the summary message added.
        summary_chars: usize,
    },
    /// Fired just before returning from `Agent::run()`.
    SessionEnd {
        /// The outcome that will be returned.
        outcome: &'a AgentOutcome,
    },
}

/// A lifecycle hook that can observe and influence agent behaviour.
pub trait Hook: Send + Sync {
    /// Called when a lifecycle event occurs.
    ///
    /// Return `HookAction::Continue` to proceed normally.
    /// Return `HookAction::Skip` or `HookAction::Error` from a `PreToolCall`
    /// event to prevent tool execution. For all other event types, `Skip`
    /// and `Error` are treated as `Continue`.
    fn on_event(&self, event: HookEvent) -> HookAction;
}

/// A registry of hooks that dispatches events to all registered hooks in order.
///
/// Hooks are stored as `Arc<dyn Hook>` and dispatched sequentially. If any
/// hook returns `HookAction::Skip` or `HookAction::Error` from a `PreToolCall`
/// event, the first non-`Continue` action is returned and remaining hooks
/// are not called for that event.
#[derive(Clone, Default)]
pub struct HookRegistry {
    hooks: Vec<Arc<dyn Hook>>,
}

impl HookRegistry {
    /// Create an empty hook registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a hook. Hooks fire in registration order.
    pub fn register(&mut self, hook: Arc<dyn Hook>) {
        self.hooks.push(hook);
    }

    /// Dispatch an event to all registered hooks.
    ///
    /// Returns the first non-`Continue` action, or `Continue` if all hooks
    /// agree. For non-`PreToolCall` events, `Skip` and `Error` are converted
    /// to `Continue`.
    pub fn dispatch(&self, event: HookEvent) -> HookAction {
        let is_pre_tool = matches!(event, HookEvent::PreToolCall { .. });
        for hook in &self.hooks {
            match hook.on_event(event.clone()) {
                HookAction::Continue => continue,
                HookAction::Skip if is_pre_tool => return HookAction::Skip,
                HookAction::Error(msg) if is_pre_tool => return HookAction::Error(msg),
                // Non-PreToolCall events: Skip/Error treated as Continue
                _ => continue,
            }
        }
        HookAction::Continue
    }

    /// Returns true if no hooks are registered.
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Returns the number of registered hooks.
    pub fn len(&self) -> usize {
        self.hooks.len()
    }
}

/// A hook that prints tool call timing information to stderr.
///
/// On `PostToolCall` events, prints `[hook] {name} took {duration_ms}ms`.
/// All other events return `HookAction::Continue`.
pub struct ToolTimingHook {
    start_times: std::sync::Mutex<HashMap<String, Instant>>,
}

impl ToolTimingHook {
    pub fn new() -> Self {
        Self {
            start_times: std::sync::Mutex::new(HashMap::new()),
        }
    }
}

impl Default for ToolTimingHook {
    fn default() -> Self {
        Self::new()
    }
}

impl Hook for ToolTimingHook {
    fn on_event(&self, event: HookEvent) -> HookAction {
        match event {
            HookEvent::PreToolCall { name, .. } => {
                let mut map = self.start_times.lock().unwrap();
                map.insert(name.to_string(), Instant::now());
                HookAction::Continue
            }
            HookEvent::PostToolCall {
                name, duration_ms, ..
            } => {
                eprintln!("[hook] {name} took {duration_ms}ms");
                HookAction::Continue
            }
            _ => HookAction::Continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct SkipHook;

    impl Hook for SkipHook {
        fn on_event(&self, event: HookEvent) -> HookAction {
            match event {
                HookEvent::PreToolCall { .. } => HookAction::Skip,
                _ => HookAction::Continue,
            }
        }
    }

    struct ErrorHook;

    impl Hook for ErrorHook {
        fn on_event(&self, event: HookEvent) -> HookAction {
            match event {
                HookEvent::PreToolCall { .. } => HookAction::Error("nope".into()),
                _ => HookAction::Continue,
            }
        }
    }

    #[test]
    fn empty_registry_returns_continue() {
        let reg = HookRegistry::new();
        let action = reg.dispatch(HookEvent::SessionStart { goal: "test" });
        assert!(matches!(action, HookAction::Continue));
    }

    #[test]
    fn session_start_fires_with_correct_goal() {
        let captured = Arc::new(std::sync::Mutex::new(String::new()));
        let c = captured.clone();
        struct GoalCapture(Arc<std::sync::Mutex<String>>);
        impl Hook for GoalCapture {
            fn on_event(&self, event: HookEvent) -> HookAction {
                if let HookEvent::SessionStart { goal } = event {
                    *self.0.lock().unwrap() = goal.to_string();
                }
                HookAction::Continue
            }
        }
        let mut reg = HookRegistry::new();
        reg.register(Arc::new(GoalCapture(c)));
        reg.dispatch(HookEvent::SessionStart { goal: "my goal" });
        assert_eq!(*captured.lock().unwrap(), "my goal");
    }

    #[test]
    fn pre_tool_call_skip_prevents_execution() {
        let mut reg = HookRegistry::new();
        reg.register(Arc::new(SkipHook));
        let action = reg.dispatch(HookEvent::PreToolCall {
            name: "write_file",
            args: &serde_json::json!({"path": "foo.txt"}),
        });
        assert!(matches!(action, HookAction::Skip));
    }

    #[test]
    fn pre_tool_call_error_returns_message() {
        let mut reg = HookRegistry::new();
        reg.register(Arc::new(ErrorHook));
        let action = reg.dispatch(HookEvent::PreToolCall {
            name: "write_file",
            args: &serde_json::json!({"path": "foo.txt"}),
        });
        assert!(matches!(action, HookAction::Error(ref msg) if msg == "nope"));
    }

    #[test]
    fn skip_and_error_on_non_pre_tool_are_continue() {
        let mut reg = HookRegistry::new();
        reg.register(Arc::new(SkipHook));
        reg.register(Arc::new(ErrorHook));
        // SessionStart — SkipHook returns Continue, ErrorHook returns Continue
        let action = reg.dispatch(HookEvent::SessionStart { goal: "test" });
        assert!(matches!(action, HookAction::Continue));
        // PostToolCall — same
        let action = reg.dispatch(HookEvent::PostToolCall {
            name: "read_file",
            args: &serde_json::json!({"path": "foo.txt"}),
            result: "ok",
            duration_ms: 5,
        });
        assert!(matches!(action, HookAction::Continue));
    }

    #[test]
    fn multiple_hooks_fire_in_order() {
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        let o1 = order.clone();
        let o2 = order.clone();

        struct OrdHook(usize, Arc<std::sync::Mutex<Vec<usize>>>);
        impl Hook for OrdHook {
            fn on_event(&self, _event: HookEvent) -> HookAction {
                self.1.lock().unwrap().push(self.0);
                HookAction::Continue
            }
        }

        let mut reg = HookRegistry::new();
        reg.register(Arc::new(OrdHook(1, o1)));
        reg.register(Arc::new(OrdHook(2, o2)));
        reg.dispatch(HookEvent::SessionStart { goal: "test" });
        assert_eq!(*order.lock().unwrap(), vec![1, 2]);
    }

    #[test]
    fn first_skip_short_circuits_remaining_hooks() {
        let count = Arc::new(AtomicUsize::new(0));
        let c1 = count.clone();
        let c2 = count.clone();

        struct FirstSkip(Arc<AtomicUsize>);
        impl Hook for FirstSkip {
            fn on_event(&self, event: HookEvent) -> HookAction {
                match event {
                    HookEvent::PreToolCall { .. } => HookAction::Skip,
                    _ => {
                        self.0.fetch_add(1, Ordering::SeqCst);
                        HookAction::Continue
                    }
                }
            }
        }

        struct SecondCounter(Arc<AtomicUsize>);
        impl Hook for SecondCounter {
            fn on_event(&self, _event: HookEvent) -> HookAction {
                self.0.fetch_add(1, Ordering::SeqCst);
                HookAction::Continue
            }
        }

        let mut reg = HookRegistry::new();
        reg.register(Arc::new(FirstSkip(c1)));
        reg.register(Arc::new(SecondCounter(c2.clone())));
        let action = reg.dispatch(HookEvent::PreToolCall {
            name: "write_file",
            args: &serde_json::json!({"path": "foo.txt"}),
        });
        assert!(matches!(action, HookAction::Skip));
        // SecondCounter should NOT have been called
        assert_eq!(c2.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn post_tool_call_receives_correct_fields() {
        let captured = Arc::new(std::sync::Mutex::new(None::<(String, String, u64)>));
        let c = captured.clone();
        struct CaptureHook(Arc<std::sync::Mutex<Option<(String, String, u64)>>>);
        impl Hook for CaptureHook {
            fn on_event(&self, event: HookEvent) -> HookAction {
                if let HookEvent::PostToolCall {
                    name,
                    result,
                    duration_ms,
                    ..
                } = event
                {
                    *self.0.lock().unwrap() =
                        Some((name.to_string(), result.to_string(), duration_ms));
                }
                HookAction::Continue
            }
        }
        let mut reg = HookRegistry::new();
        reg.register(Arc::new(CaptureHook(c)));
        reg.dispatch(HookEvent::PostToolCall {
            name: "read_file",
            args: &serde_json::json!({"path": "foo.txt"}),
            result: "file contents",
            duration_ms: 42,
        });
        let captured = captured.lock().unwrap().clone().unwrap();
        assert_eq!(captured.0, "read_file");
        assert_eq!(captured.1, "file contents");
        assert_eq!(captured.2, 42);
    }

    #[test]
    fn session_end_receives_outcome() {
        let captured = Arc::new(std::sync::Mutex::new(None));
        let c = captured.clone();
        struct CaptureOutcome(Arc<std::sync::Mutex<Option<AgentOutcome>>>);
        impl Hook for CaptureOutcome {
            fn on_event(&self, event: HookEvent) -> HookAction {
                if let HookEvent::SessionEnd { outcome } = event {
                    *self.0.lock().unwrap() = Some(outcome.clone());
                }
                HookAction::Continue
            }
        }
        let outcome = AgentOutcome {
            final_message: Some("done".into()),
            transcript: vec![],
            steps: 3,
            finish: crate::agent::FinishReason::NoMoreToolCalls,
            total_usage: crate::llm::TokenUsage::default(),
            total_llm_latency_ms: 100,
        };
        let mut reg = HookRegistry::new();
        reg.register(Arc::new(CaptureOutcome(c)));
        reg.dispatch(HookEvent::SessionEnd { outcome: &outcome });
        let captured = captured.lock().unwrap().take().unwrap();
        assert_eq!(captured.final_message.as_deref(), Some("done"));
        assert_eq!(captured.steps, 3);
    }

    #[test]
    fn pre_compact_receives_transcript_len() {
        let captured = Arc::new(std::sync::Mutex::new(0usize));
        let c = captured.clone();
        struct CaptureLen(Arc<std::sync::Mutex<usize>>);
        impl Hook for CaptureLen {
            fn on_event(&self, event: HookEvent) -> HookAction {
                if let HookEvent::PreCompact { transcript_len } = event {
                    *self.0.lock().unwrap() = transcript_len;
                }
                HookAction::Continue
            }
        }
        let mut reg = HookRegistry::new();
        reg.register(Arc::new(CaptureLen(c)));
        reg.dispatch(HookEvent::PreCompact {
            transcript_len: 5000,
        });
        assert_eq!(*captured.lock().unwrap(), 5000);
    }

    #[test]
    fn post_compact_receives_removed_and_summary_chars() {
        let captured = Arc::new(std::sync::Mutex::new((0usize, 0usize)));
        let c = captured.clone();
        struct CaptureCompact(Arc<std::sync::Mutex<(usize, usize)>>);
        impl Hook for CaptureCompact {
            fn on_event(&self, event: HookEvent) -> HookAction {
                if let HookEvent::PostCompact {
                    removed,
                    summary_chars,
                } = event
                {
                    *self.0.lock().unwrap() = (removed, summary_chars);
                }
                HookAction::Continue
            }
        }
        let mut reg = HookRegistry::new();
        reg.register(Arc::new(CaptureCompact(c)));
        reg.dispatch(HookEvent::PostCompact {
            removed: 10,
            summary_chars: 200,
        });
        assert_eq!(captured.lock().unwrap().0, 10);
        assert_eq!(captured.lock().unwrap().1, 200);
    }

    #[test]
    fn hook_event_is_non_exhaustive() {
        // Compile-time check: HookEvent is #[non_exhaustive]
        let _ = HookEvent::SessionStart { goal: "test" };
    }

    #[test]
    fn tool_timing_hook_prints_to_stderr_on_post_tool_call() {
        // Capture stderr by redirecting
        let hook = ToolTimingHook::new();
        let action = hook.on_event(HookEvent::PostToolCall {
            name: "read_file",
            args: &serde_json::json!({"path": "foo.txt"}),
            result: "ok",
            duration_ms: 42,
        });
        assert!(matches!(action, HookAction::Continue));
    }

    #[test]
    fn tool_timing_hook_returns_continue_for_non_tool_events() {
        let hook = ToolTimingHook::new();
        let action = hook.on_event(HookEvent::SessionStart { goal: "test" });
        assert!(matches!(action, HookAction::Continue));

        let action = hook.on_event(HookEvent::PreCompact {
            transcript_len: 100,
        });
        assert!(matches!(action, HookAction::Continue));

        let action = hook.on_event(HookEvent::PostCompact {
            removed: 5,
            summary_chars: 50,
        });
        assert!(matches!(action, HookAction::Continue));

        let outcome = AgentOutcome {
            final_message: Some("done".into()),
            transcript: vec![],
            steps: 1,
            finish: crate::agent::FinishReason::NoMoreToolCalls,
            total_usage: crate::llm::TokenUsage::default(),
            total_llm_latency_ms: 0,
        };
        let action = hook.on_event(HookEvent::SessionEnd { outcome: &outcome });
        assert!(matches!(action, HookAction::Continue));
    }

    #[test]
    fn tool_timing_hook_pre_tool_call_returns_continue() {
        let hook = ToolTimingHook::new();
        let action = hook.on_event(HookEvent::PreToolCall {
            name: "write_file",
            args: &serde_json::json!({"path": "foo.txt"}),
        });
        assert!(matches!(action, HookAction::Continue));
    }
}
