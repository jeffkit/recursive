//! End-to-end smoke: runtime + real filesystem tools + scripted mock model.
//!
//! Verifies that the kernel can actually drive a coding-style task to
//! completion using only its public API, with no network and no real LLM.

use std::sync::Arc;

use recursive::{
    llm::{Completion, MockProvider, ToolCall},
    runtime::AgentRuntime,
    tools::{GlobTool, LocalTransport, ReadFile, ToolTransport, WriteFile},
    ToolRegistry,
};
use serde_json::json;
use tempfile::TempDir;

#[tokio::test]
async fn runtime_writes_reads_and_summarises() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let script = vec![
        Completion {
            content: "I'll create greet.txt then read it back.".into(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "Write".into(),
                arguments: json!({"path": "greet.txt", "contents": "hello recursive"}),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "".into(),
            tool_calls: vec![ToolCall {
                id: "c2".into(),
                name: "Read".into(),
                arguments: json!({"path": "greet.txt"}),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "".into(),
            tool_calls: vec![ToolCall {
                id: "c3".into(),
                name: "Glob".into(),
                arguments: json!({"pattern": "*.txt"}),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "Created greet.txt containing 'hello recursive'.".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
    ];

    let llm = Arc::new(MockProvider::new(script));
    let transport: Arc<dyn ToolTransport> = Arc::new(LocalTransport);
    let tools = ToolRegistry::new(transport)
        .register(Arc::new(WriteFile::new(root)))
        .register(Arc::new(ReadFile::new(root)))
        .register(Arc::new(GlobTool::new(root)));

    let mut runtime = AgentRuntime::builder()
        .llm(llm)
        .tools(tools)
        .system_prompt("you are a test runtime")
        .max_steps(10)
        .build()
        .unwrap();

    let outcome = runtime.run("create greet.txt and confirm").await.unwrap();

    assert_eq!(outcome.steps, 4);
    assert!(outcome.final_text.as_deref().unwrap().contains("greet.txt"));

    // The runtime actually wrote the file via the real fs tool.
    let on_disk = std::fs::read_to_string(root.join("greet.txt")).unwrap();
    assert_eq!(on_disk, "hello recursive");
}
