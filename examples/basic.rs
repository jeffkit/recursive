//! Basic agent example: create a runtime with a mock LLM, run it, and inspect
//! the outcome. No API key required — uses `MockProvider` for offline testing.

use recursive::runtime::AgentRuntime;
use recursive::llm::{Completion, MockProvider};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    // Create a mock provider that returns a single completion.
    let provider = Arc::new(MockProvider::new(vec![Completion {
        content: "Hello from the agent!".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));

    // Build the runtime with the mock provider.
    let mut runtime = AgentRuntime::builder()
        .llm(provider)
        .system_prompt("You are a helpful assistant.")
        .max_steps(5)
        .build()
        .expect("failed to build runtime");

    // Run the agent with a goal.
    let outcome = runtime.run("Say hello").await.expect("agent run failed");

    println!("Final message: {:?}", outcome.final_text);
    println!("Steps taken: {}", outcome.steps);
    println!("Finish reason: {:?}", outcome.finish_reason);
    println!("Transcript messages: {}", runtime.transcript().len());
}
