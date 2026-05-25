//! End-to-end smoke: agent + real filesystem tools + scripted mock model.
//!
//! Verifies that the kernel can actually drive a coding-style task to
//! completion using only its public API, with no network and no real LLM.

use std::sync::Arc;

use recursive::{
    llm::{Completion, MockProvider, ToolCall},
    tools::{ListDir, ReadFile, WriteFile},
    Agent, ToolRegistry,
};
use serde_json::json;
use tempfile::TempDir;

#[tokio::test]
async fn agent_writes_reads_and_summarises() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let script = vec![
        Completion {
            content: "I'll create greet.txt then read it back.".into(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                arguments: json!({"path": "greet.txt", "contents": "hello recursive"}),
            }],
            finish_reason: Some("tool_calls".into()),
        },
        Completion {
            content: "".into(),
            tool_calls: vec![ToolCall {
                id: "c2".into(),
                name: "read_file".into(),
                arguments: json!({"path": "greet.txt"}),
            }],
            finish_reason: Some("tool_calls".into()),
        },
        Completion {
            content: "".into(),
            tool_calls: vec![ToolCall {
                id: "c3".into(),
                name: "list_dir".into(),
                arguments: json!({"path": "."}),
            }],
            finish_reason: Some("tool_calls".into()),
        },
        Completion {
            content: "Created greet.txt containing 'hello recursive'.".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
        },
    ];

    let llm = Arc::new(MockProvider::new(script));
    let tools = ToolRegistry::new()
        .register(Arc::new(WriteFile::new(root)))
        .register(Arc::new(ReadFile::new(root)))
        .register(Arc::new(ListDir::new(root)));

    let mut agent = Agent::builder()
        .llm(llm)
        .tools(tools)
        .system_prompt("you are a test agent")
        .max_steps(10)
        .build()
        .unwrap();

    let outcome = agent.run("create greet.txt and confirm").await.unwrap();

    assert_eq!(outcome.steps, 4);
    assert!(outcome.final_message.as_deref().unwrap().contains("greet.txt"));

    // The agent actually wrote the file via the real fs tool.
    let on_disk = std::fs::read_to_string(root.join("greet.txt")).unwrap();
    assert_eq!(on_disk, "hello recursive");
}
