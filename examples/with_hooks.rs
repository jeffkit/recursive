//! Agent with lifecycle hooks: observe and log agent events.
//! Uses `MockProvider` so no API key is needed.

use recursive::agent::Agent;
use recursive::hooks::{Hook, HookAction, HookEvent, HookRegistry};
use recursive::llm::{Completion, MockProvider};
use std::sync::Arc;

/// A simple hook that logs every event to stdout.
struct LoggingHook;

impl Hook for LoggingHook {
    fn on_event(&self, event: HookEvent) -> HookAction {
        match event {
            HookEvent::SessionStart { goal } => {
                println!("[hook] Session started — goal: {goal}");
            }
            HookEvent::PreToolCall { name, args } => {
                println!("[hook] About to call tool '{name}' with args: {args}");
            }
            HookEvent::PostToolCall {
                name, duration_ms, ..
            } => {
                println!("[hook] Tool '{name}' completed in {duration_ms}ms");
            }
            HookEvent::PreCompact { transcript_len } => {
                println!("[hook] Compaction triggered (transcript: {transcript_len} chars)");
            }
            HookEvent::PostCompact {
                removed,
                summary_chars,
                ..
            } => {
                println!("[hook] Compacted {removed} messages, summary: {summary_chars} chars");
            }
            HookEvent::SessionEnd { outcome } => {
                println!(
                    "[hook] Session ended — steps: {}, finish: {:?}",
                    outcome.steps, outcome.finish
                );
            }
            _ => {}
        }
        HookAction::Continue
    }
}

#[tokio::main]
async fn main() {
    // Register the logging hook.
    let mut hooks = HookRegistry::new();
    hooks.register(Arc::new(LoggingHook));

    let provider = Arc::new(MockProvider::new(vec![Completion {
        content: "Hello from the agent!".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
    }]));

    let mut agent = Agent::builder()
        .llm(provider)
        .system_prompt("You are a helpful assistant.")
        .max_steps(5)
        .hook(Arc::new(LoggingHook))
        .build()
        .expect("failed to build agent");

    let outcome = agent.run("Say hello").await.expect("agent run failed");
    println!("\nFinal message: {:?}", outcome.final_message);
}
