//! Anthropic provider integration smoke test.
//!
//! Verifies that the AnthropicProvider plumbing works end-to-end with a
//! mock HTTP server, exercising the full request/response cycle including
//! tool calls and tool results.

use std::sync::Arc;

use recursive::{
    llm::AnthropicProvider,
    tools::{LocalTransport, ReadFile, ToolTransport, WriteFile},
    Agent, ToolRegistry,
};
use tempfile::TempDir;

#[tokio::test]
async fn anthropic_smoke_constructs_with_minimum_config() {
    // Just verify construction succeeds with realistic args
    let provider = AnthropicProvider::new(
        "https://api.example.com",
        "sk-test-key",
        "claude-3-sonnet-20240229",
    );
    // Provider should be usable (no panic on construction)
    let _ = provider;
}

#[tokio::test]
async fn anthropic_full_agent_loop_with_mock_provider() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // The agent will make two LLM calls:
    // 1. First call: model says "I'll write a file" and calls write_file
    // 2. Second call: model says "Done" and stops
    //
    // We need two mock responses. The agent loop calls complete() once per
    // step, so we set up a mock server that returns the first response,
    // then the agent calls complete again and we return the second.

    // For this test we use a simpler approach: spawn a mock server that
    // returns a tool_use response, then another for the final response.
    // Since the agent loop calls complete() sequentially, we use a shared
    // counter to serve different responses.

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    let call_count = Arc::new(AtomicUsize::new(0));
    let responses: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![
        // Response 1: tool_use for write_file
        r#"{"type":"message","content":[{"type":"tool_use","id":"call_abc","name":"write_file","input":{"path":"hello.txt","contents":"Hello from Anthropic"}}],"stop_reason":"tool_use","usage":{"input_tokens":50,"output_tokens":30}}"#.to_string(),
        // Response 2: text response saying done
        r#"{"type":"message","content":[{"type":"text","text":"Created hello.txt with content."}],"stop_reason":"end_turn","usage":{"input_tokens":60,"output_tokens":10}}"#.to_string(),
    ]));

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let responses_clone = responses.clone();
    let count_clone = call_count.clone();

    let handle = std::thread::spawn(move || {
        // Accept two connections (one per LLM call)
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::{Read, Write};
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);

            let idx = count_clone.fetch_add(1, Ordering::SeqCst);
            let body = &responses_clone.lock().unwrap()[idx];
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            write!(stream, "{}", response).unwrap();
            stream.flush().unwrap();
        }
    });

    // Give the server a moment to start
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let provider = Arc::new(AnthropicProvider::new(
        format!("http://{addr}"),
        "sk-noop",
        "claude-3-sonnet-20240229",
    ));

    let transport: Arc<dyn ToolTransport> = Arc::new(LocalTransport);
    let tools = ToolRegistry::new(transport)
        .register(Arc::new(WriteFile::new(root)))
        .register(Arc::new(ReadFile::new(root)));

    let mut agent = Agent::builder()
        .llm(provider)
        .tools(tools)
        .system_prompt("you are a test agent using Anthropic API")
        .max_steps(5)
        .build()
        .unwrap();

    let outcome = agent.run("create hello.txt with content").await.unwrap();

    handle.join().unwrap();

    assert_eq!(outcome.steps, 2);
    assert!(outcome
        .final_message
        .as_deref()
        .unwrap()
        .contains("hello.txt"));

    // The agent actually wrote the file via the real fs tool.
    let on_disk = std::fs::read_to_string(root.join("hello.txt")).unwrap();
    assert_eq!(on_disk, "Hello from Anthropic");
}
