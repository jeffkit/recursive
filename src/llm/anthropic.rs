//! Anthropic Messages API adapter.
//!
//! Targets the `/v1/messages` endpoint that Anthropic and compatible
//! providers (MiniMax, DeepSeek) speak.

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

use super::{Completion, LlmProvider, TokenUsage, ToolCall, ToolSpec};
use crate::error::{Error, Result};
use crate::message::{Message, Role};

/// Retry policy for transient failures (network timeouts, 5xx errors).
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_retries: usize,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(8),
        }
    }
}

impl RetryPolicy {
    /// Decide whether the caller should wait and try again.
    pub fn backoff_for(
        &self,
        attempt: usize,
        status: Option<u16>,
        is_network_error: bool,
    ) -> Option<Duration> {
        if attempt >= self.max_retries {
            return None;
        }

        let is_transient = is_network_error || status.is_some_and(|s| (500..600).contains(&s));

        if !is_transient {
            return None;
        }

        let backoff = self.initial_backoff * 2u32.pow(attempt as u32);
        Some(backoff.min(self.max_backoff))
    }
}

#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    base_url: String,
    api_key: String,
    model: String,
    client: Client,
    temperature: f64,
    max_tokens: u32,
    retry: RetryPolicy,
}

impl AnthropicProvider {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
            client: Client::builder()
                .timeout(Duration::from_secs(180))
                .build()
                .expect("reqwest client build"),
            temperature: 0.2,
            max_tokens: 4096,
            retry: RetryPolicy::default(),
        }
    }

    /// Build an `Error::Llm` with the model name prefixed.
    fn make_err(&self, ctx: impl Into<String>) -> Error {
        Error::Llm(format!("model={}: {}", self.model, ctx.into()))
    }

    pub fn with_temperature(mut self, t: f64) -> Self {
        self.temperature = t;
        self
    }

    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }

    pub fn with_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.retry = policy;
        self
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn complete(&self, messages: &[Message], tools: &[ToolSpec]) -> Result<Completion> {
        let (system, messages) = extract_system_message(messages);

        // Anthropic requires messages to not start with assistant role
        let messages = filter_leading_assistant(&messages);

        let body = build_request(
            &self.model,
            self.temperature,
            self.max_tokens,
            system.as_deref(),
            &messages,
            tools,
        );
        let url = format!("{}/v1/messages", self.base_url);

        let mut attempt = 0;
        loop {
            tracing::debug!(target: "recursive::llm", request = %body, "POST {}", url);
            let result = self
                .client
                .post(&url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .json(&body)
                .send()
                .await;

            match result {
                Ok(resp) => {
                    let status = resp.status();
                    let is_network_error = false;

                    if status.is_success() {
                        let text = resp.text().await?;
                        let parsed: AnthropicResponse =
                            serde_json::from_str(&text).map_err(|e| {
                                self.make_err(format!(
                                    "failed to parse response: {e}; body: {text}"
                                ))
                            })?;
                        return Ok(parse_completion(parsed));
                    }

                    // Non-2xx response: check if it's transient (5xx)
                    let text = resp.text().await?;
                    tracing::debug!(target: "recursive::llm", body = %text, "error response");

                    if let Some(backoff) =
                        self.retry
                            .backoff_for(attempt, Some(status.as_u16()), is_network_error)
                    {
                        tracing::warn!(
                            target: "recursive::llm",
                            attempt,
                            backoff_ms = backoff.as_millis(),
                            status = status.as_u16(),
                            "transient HTTP error, retrying"
                        );
                        tokio::time::sleep(backoff).await;
                        attempt += 1;
                        continue;
                    }

                    // Non-transient (4xx or other)
                    return Err(self.make_err(format!("HTTP {}: {}", status, text)));
                }
                Err(e) => {
                    // Network error
                    if let Some(backoff) = self.retry.backoff_for(attempt, None, true) {
                        tracing::warn!(
                            target: "recursive::llm",
                            attempt,
                            backoff_ms = backoff.as_millis(),
                            error = %e,
                            "network error, retrying"
                        );
                        tokio::time::sleep(backoff).await;
                        attempt += 1;
                        continue;
                    }

                    return Err(self.make_err(format!("request failed: {e}")));
                }
            }
        }
    }
}

/// Extract system message if present, return (system_content, remaining_messages).
fn extract_system_message(messages: &[Message]) -> (Option<String>, Vec<Message>) {
    if messages.first().is_some_and(|m| m.role == Role::System) {
        let system = messages[0].content.clone();
        let rest = messages[1..].to_vec();
        (Some(system), rest)
    } else {
        (None, messages.to_vec())
    }
}

/// Filter out any leading assistant messages that would cause Anthropic to reject the request.
/// Anthropic requires the first message to be user, or system+user.
fn filter_leading_assistant(messages: &[Message]) -> Vec<Message> {
    let mut result = messages.to_vec();
    while result
        .first()
        .is_some_and(|m| m.role == Role::Assistant && m.tool_calls.is_empty())
    {
        // Remove leading assistant message with no tool calls
        result.remove(0);
    }
    result
}

fn build_request(
    model: &str,
    temperature: f64,
    max_tokens: u32,
    system: Option<&str>,
    messages: &[Message],
    tools: &[ToolSpec],
) -> Value {
    let mut req = serde_json::json!({
        "model": model,
        "max_tokens": max_tokens,
        "temperature": temperature,
    });

    if let Some(sys) = system {
        req["system"] = Value::String(sys.to_string());
    }

    let msgs: Vec<Value> = messages.iter().map(serialize_message).collect();
    req["messages"] = Value::Array(msgs);

    if !tools.is_empty() {
        let tools_json: Vec<Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                })
            })
            .collect();
        req["tools"] = Value::Array(tools_json);
    }

    req
}

fn serialize_message(m: &Message) -> Value {
    let role = match m.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "user", // Anthropic uses "user" for tool results
    };

    // If this message has tool calls, format as tool_use blocks
    if !m.tool_calls.is_empty() {
        let tool_uses: Vec<Value> = m
            .tool_calls
            .iter()
            .map(|c| {
                serde_json::json!({
                    "type": "tool_use",
                    "id": c.id,
                    "name": c.name,
                    "input": c.arguments,
                })
            })
            .collect();
        serde_json::json!({
            "role": role,
            "content": tool_uses,
        })
    } else if let Some(id) = &m.tool_call_id {
        // Tool result message - format as tool_result content block
        serde_json::json!({
            "role": role,
            "content": [{
                "type": "tool_result",
                "tool_use_id": id,
                "content": m.content,
            }],
        })
    } else {
        // Regular text message
        serde_json::json!({
            "role": role,
            "content": m.content,
        })
    }
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    #[serde(default)]
    content: Vec<ContentBlock>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "text")]
    Text {
        #[serde(default)]
        text: String,
    },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: Value,
    },
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: Option<u32>,
    #[serde(default)]
    output_tokens: Option<u32>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
}

impl AnthropicUsage {
    fn to_token_usage(&self) -> TokenUsage {
        TokenUsage {
            prompt_tokens: self.input_tokens.unwrap_or(0),
            completion_tokens: self.output_tokens.unwrap_or(0),
            total_tokens: self
                .input_tokens
                .unwrap_or(0)
                .saturating_add(self.output_tokens.unwrap_or(0)),
            cache_hit_tokens: self.cache_read_input_tokens.unwrap_or(0),
            cache_miss_tokens: self.cache_creation_input_tokens.unwrap_or(0),
        }
    }
}

fn parse_completion(response: AnthropicResponse) -> Completion {
    let mut content = String::new();
    let mut tool_calls = Vec::new();

    for block in response.content {
        match block {
            ContentBlock::Text { text } => {
                content.push_str(&text);
            }
            ContentBlock::ToolUse { id, name, input } => {
                tool_calls.push(ToolCall {
                    id,
                    name,
                    arguments: input,
                });
            }
        }
    }

    let finish_reason = response.stop_reason.map(|r| match r.as_str() {
        "end_turn" => "stop".to_string(),
        "max_tokens" => "length".to_string(),
        "tool_use" => "tool_calls".to_string(),
        other => other.to_string(),
    });

    Completion {
        content,
        tool_calls,
        finish_reason,
        usage: response.usage.map(|u| u.to_token_usage()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_system_message_when_present() {
        let msgs = vec![
            Message::system("You are helpful.".to_string()),
            Message::user("Hello".to_string()),
        ];
        let (system, rest) = extract_system_message(&msgs);
        assert_eq!(system, Some("You are helpful.".to_string()));
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].role, Role::User);
    }

    #[test]
    fn extract_system_message_when_not_present() {
        let msgs = vec![Message::user("Hello".to_string())];
        let (system, rest) = extract_system_message(&msgs);
        assert_eq!(system, None);
        assert_eq!(rest.len(), 1);
    }

    #[test]
    fn filter_leading_assistant_removes_empty_assistant() {
        let msgs = vec![
            Message::assistant("Hi".to_string()),
            Message::user("Hello".to_string()),
        ];
        let filtered = filter_leading_assistant(&msgs);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].role, Role::User);
    }

    #[test]
    fn filter_leading_assistant_keeps_assistant_with_tool_calls() {
        let msgs = vec![
            Message::assistant_with_tool_calls(
                "I'll use a tool".to_string(),
                vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "test".to_string(),
                    arguments: Value::Object(Default::default()),
                }],
            ),
            Message::user("Hello".to_string()),
        ];
        let filtered = filter_leading_assistant(&msgs);
        // Should keep the assistant message because it has tool_calls
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn parses_text_response() {
        let raw = r#"{
            "type": "message",
            "content": [{"type": "text", "text": "Hello"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        }"#;
        let parsed: AnthropicResponse = serde_json::from_str(raw).unwrap();
        let c = parse_completion(parsed);
        assert_eq!(c.content, "Hello");
        assert!(c.tool_calls.is_empty());
        assert_eq!(c.finish_reason, Some("stop".to_string()));
        assert!(c.usage.is_some());
    }

    #[test]
    fn parses_tool_use_response() {
        let raw = r#"{
            "type": "message",
            "content": [
                {"type": "tool_use", "id": "call_1", "name": "read_file", "input": {"path": "src/lib.rs"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 100, "output_tokens": 50}
        }"#;
        let parsed: AnthropicResponse = serde_json::from_str(raw).unwrap();
        let c = parse_completion(parsed);
        assert!(c.content.is_empty());
        assert_eq!(c.tool_calls.len(), 1);
        assert_eq!(c.tool_calls[0].id, "call_1");
        assert_eq!(c.tool_calls[0].name, "read_file");
        assert_eq!(c.tool_calls[0].arguments["path"], "src/lib.rs");
    }

    #[test]
    fn parses_mixed_content_response() {
        let raw = r#"{
            "type": "message",
            "content": [
                {"type": "text", "text": "I'll read that file. "},
                {"type": "tool_use", "id": "call_1", "name": "read_file", "input": {"path": "src/lib.rs"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 50, "output_tokens": 25}
        }"#;
        let parsed: AnthropicResponse = serde_json::from_str(raw).unwrap();
        let c = parse_completion(parsed);
        assert_eq!(c.content, "I'll read that file. ");
        assert_eq!(c.tool_calls.len(), 1);
    }

    #[test]
    fn parses_usage_correctly() {
        let raw = r#"{
            "type": "message",
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 50,
                "cache_creation_input_tokens": 20,
                "cache_read_input_tokens": 30
            }
        }"#;
        let parsed: AnthropicResponse = serde_json::from_str(raw).unwrap();
        let c = parse_completion(parsed);
        let u = c.usage.unwrap();
        assert_eq!(u.prompt_tokens, 100);
        assert_eq!(u.completion_tokens, 50);
        assert_eq!(u.cache_hit_tokens, 30);
        assert_eq!(u.cache_miss_tokens, 20);
    }

    #[test]
    fn serializes_tool_result_message() {
        let msg = Message::tool_result("call_123".to_string(), "result content".to_string());
        let v = serialize_message(&msg);
        assert_eq!(v["role"], "user");
        let content = &v["content"][0];
        assert_eq!(content["type"], "tool_result");
        assert_eq!(content["tool_use_id"], "call_123");
        assert_eq!(content["content"], "result content");
    }

    #[test]
    fn serializes_assistant_with_tool_calls() {
        let msg = Message::assistant_with_tool_calls(
            "I'll use a tool".to_string(),
            vec![ToolCall {
                id: "abc".to_string(),
                name: "write_file".to_string(),
                arguments: serde_json::json!({"path": "a", "contents": "b"}),
            }],
        );
        let v = serialize_message(&msg);
        assert_eq!(v["role"], "assistant");
        let content = &v["content"][0];
        assert_eq!(content["type"], "tool_use");
        assert_eq!(content["name"], "write_file");
    }

    #[test]
    fn builds_request_with_system() {
        let req = build_request(
            "claude-3",
            0.2,
            4096,
            Some("You are helpful."),
            &[Message::user("Hi".to_string())],
            &[],
        );
        assert_eq!(req["system"], "You are helpful.");
        assert_eq!(req["model"], "claude-3");
    }

    #[test]
    fn builds_request_without_system() {
        let req = build_request(
            "claude-3",
            0.2,
            4096,
            None,
            &[Message::user("Hi".to_string())],
            &[],
        );
        assert!(req.get("system").is_none());
    }

    #[test]
    fn builds_request_with_tools() {
        let tools = vec![ToolSpec {
            name: "test".to_string(),
            description: "A test tool".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let req = build_request(
            "claude-3",
            0.2,
            4096,
            None,
            &[Message::user("Hi".to_string())],
            &tools,
        );
        assert!(req.get("tools").is_some());
    }

    #[tokio::test]
    async fn test_a_mock_server_returns_canned_response() {
        // Spawn a mock server that returns a valid Anthropic response
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::{Read, Write};
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);

            let body = r#"{"type":"message","content":[{"type":"text","text":"Hello from Anthropic"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            write!(stream, "{}", response).unwrap();
            stream.flush().unwrap();
        });

        // Give the server a moment to start
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let provider =
            AnthropicProvider::new(format!("http://{addr}"), "sk-noop", "claude-3-sonnet");
        let result = provider
            .complete(&[Message::user("hi".to_string())], &[])
            .await;

        let _ = handle.join();

        let completion = result.expect("should succeed");
        assert!(completion.content.contains("Hello from Anthropic"));
        assert!(completion.usage.is_some());
        let u = completion.usage.unwrap();
        assert_eq!(u.prompt_tokens, 10);
        assert_eq!(u.completion_tokens, 5);
    }

    #[tokio::test]
    async fn test_b_error_response_includes_model_name() {
        // Spawn a mock server that returns 401
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::{Read, Write};
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);

            let body = r#"{"error":{"type":"authentication_error","message":"Invalid API Key"}}"#;
            let response = format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            write!(stream, "{}", response).unwrap();
            stream.flush().unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let provider =
            AnthropicProvider::new(format!("http://{addr}"), "sk-invalid", "claude-3-opus");
        let err = provider
            .complete(&[Message::user("hi".to_string())], &[])
            .await
            .unwrap_err();

        let _ = handle.join();

        let msg = err.to_string();
        assert!(
            msg.contains("model=claude-3-opus"),
            "error should contain model name: {msg}"
        );
        assert!(
            msg.contains("401"),
            "error should contain HTTP status: {msg}"
        );
    }

    #[tokio::test]
    async fn test_c_tool_use_extracts_correct_tool_call() {
        // Spawn a mock server that returns a tool_use response
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::{Read, Write};
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);

            let body = r#"{"type":"message","content":[{"type":"tool_use","id":"call_abc123","name":"read_file","input":{"path":"src/lib.rs"}}],"stop_reason":"tool_use","usage":{"input_tokens":50,"output_tokens":30}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            write!(stream, "{}", response).unwrap();
            stream.flush().unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let provider =
            AnthropicProvider::new(format!("http://{addr}"), "sk-noop", "claude-3-sonnet");
        let result = provider
            .complete(&[Message::user("read the file".to_string())], &[])
            .await;

        let _ = handle.join();

        let completion = result.expect("should succeed");
        assert_eq!(completion.tool_calls.len(), 1);
        let tc = &completion.tool_calls[0];
        assert_eq!(tc.id, "call_abc123");
        assert_eq!(tc.name, "read_file");
        assert_eq!(tc.arguments["path"], "src/lib.rs");
    }

    #[test]
    fn policy_retries_5xx_with_exponential_backoff() {
        let policy = RetryPolicy::default();
        assert_eq!(
            policy.backoff_for(0, Some(503), false),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            policy.backoff_for(1, Some(500), false),
            Some(Duration::from_secs(2))
        );
        assert_eq!(policy.backoff_for(2, Some(500), false), None);
    }

    #[test]
    fn policy_retries_network_errors() {
        let policy = RetryPolicy::default();
        assert_eq!(
            policy.backoff_for(0, None, true),
            Some(Duration::from_secs(1))
        );
    }

    #[test]
    fn policy_does_not_retry_4xx() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.backoff_for(0, Some(400), false), None);
        assert_eq!(policy.backoff_for(0, Some(401), false), None);
        assert_eq!(policy.backoff_for(0, Some(404), false), None);
    }
}
