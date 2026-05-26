//! Agent with tools: register a custom tool and watch the agent call it.
//! Uses `MockProvider` so no API key is needed.

use async_trait::async_trait;
use recursive::agent::Agent;
use recursive::llm::{Completion, MockProvider, ToolCall, ToolSpec};
use recursive::tools::{Tool, ToolRegistry};
use serde_json::{json, Value};
use std::sync::Arc;

/// A simple tool that greets a person by name.
struct Greeter;

#[async_trait]
impl Tool for Greeter {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "greet".into(),
            description: "Greet a person by name".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "The name to greet" }
                },
                "required": ["name"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> recursive::Result<String> {
        let name = args["name"].as_str().unwrap_or("world");
        Ok(format!("Hello, {name}!"))
    }
}

#[tokio::main]
async fn main() {
    // Register the custom tool.
    let tools = ToolRegistry::local().register(Arc::new(Greeter));

    // Script: first call the tool, then finish.
    let provider = Arc::new(MockProvider::new(vec![
        Completion {
            content: "Let me greet you.".into(),
            tool_calls: vec![ToolCall {
                id: "call-1".into(),
                name: "greet".into(),
                arguments: json!({"name": "Alice"}),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "I greeted Alice.".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
    ]));

    let mut agent = Agent::builder()
        .llm(provider)
        .tools(tools)
        .system_prompt("You are a friendly assistant.")
        .max_steps(5)
        .build()
        .expect("failed to build agent");

    let outcome = agent.run("Greet Alice").await.expect("agent run failed");

    println!("Final message: {:?}", outcome.final_message);
    println!("Steps taken: {}", outcome.steps);
    println!("Finish reason: {:?}", outcome.finish);

    // Show the tool result from the transcript.
    for msg in &outcome.transcript {
        if msg.role == recursive::message::Role::Tool {
            println!("Tool result: {}", msg.content);
        }
    }
}
