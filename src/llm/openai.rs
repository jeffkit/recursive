//! OpenAI-compatible chat-completions adapter.
//!
//! Targets the `/chat/completions` shape that OpenAI, Azure (via gateway),
//! GLM (Zhipu), DeepSeek, Moonshot, Together, Ollama and many others speak.
//! The only thing that varies is the base URL + model name + API key, which
//! is all driven by config.

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
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
    /// `attempt` is 0-indexed (0 = the first retry decision after the
    /// initial try has failed). Returns `Some(backoff)` to retry,
    /// `None` to give up and propagate the error.
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
pub struct OpenAiProvider {
    base_url: String,
    api_key: String,
    model: String,
    client: Client,
    temperature: f64,
    retry: RetryPolicy,
}

impl OpenAiProvider {
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
            retry: RetryPolicy::default(),
        }
    }

    pub fn with_temperature(mut self, t: f64) -> Self {
        self.temperature = t;
        self
    }

    pub fn with_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.retry = policy;
        self
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn complete(&self, messages: &[Message], tools: &[ToolSpec]) -> Result<Completion> {
        let body = build_request(&self.model, self.temperature, messages, tools);
        let url = format!("{}/chat/completions", self.base_url);

        let mut attempt = 0;
        loop {
            tracing::debug!(target: "recursive::llm", request = %body, "POST {}", url);
            let result = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await;

            match result {
                Ok(resp) => {
                    let status = resp.status();
                    let is_network_error = false;

                    if status.is_success() {
                        let text = resp.text().await?;
                        let parsed: ChatResponse = serde_json::from_str(&text).map_err(|e| {
                            Error::Llm(format!("failed to parse response: {e}; body: {text}"))
                        })?;
                        let choice = parsed
                            .choices
                            .into_iter()
                            .next()
                            .ok_or_else(|| Error::Llm("response had no choices".into()))?;
                        return Ok(parse_completion(choice, parsed.usage));
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
                    return Err(Error::Llm(format!("HTTP {}: {}", status, text)));
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

                    return Err(Error::Llm(format!("request failed: {e}")));
                }
            }
        }
    }
}

fn build_request(model: &str, temperature: f64, messages: &[Message], tools: &[ToolSpec]) -> Value {
    let mut req = serde_json::json!({
        "model": model,
        "temperature": temperature,
        "messages": messages.iter().map(serialize_message).collect::<Vec<_>>(),
    });
    if !tools.is_empty() {
        let tools_json: Vec<Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect();
        req["tools"] = Value::Array(tools_json);
        req["tool_choice"] = Value::String("auto".into());
    }
    req
}

fn serialize_message(m: &Message) -> Value {
    let role = match m.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    let mut obj = serde_json::Map::new();
    obj.insert("role".into(), Value::String(role.into()));
    obj.insert("content".into(), Value::String(m.content.clone()));
    if let Some(id) = &m.tool_call_id {
        obj.insert("tool_call_id".into(), Value::String(id.clone()));
    }
    if !m.tool_calls.is_empty() {
        let calls: Vec<Value> = m
            .tool_calls
            .iter()
            .map(|c| {
                serde_json::json!({
                    "id": c.id,
                    "type": "function",
                    "function": {
                        "name": c.name,
                        "arguments": serde_json::to_string(&c.arguments).unwrap_or_else(|_| "{}".into()),
                    }
                })
            })
            .collect();
        obj.insert("tool_calls".into(), Value::Array(calls));
    }
    Value::Object(obj)
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<ResponseUsage>,
}

#[derive(Debug, Deserialize)]
struct ResponseUsage {
    #[serde(default)]
    prompt_tokens: Option<u32>,
    #[serde(default)]
    completion_tokens: Option<u32>,
    #[serde(default)]
    total_tokens: Option<u32>,
}

impl ResponseUsage {
    fn to_token_usage(&self) -> TokenUsage {
        TokenUsage {
            prompt_tokens: self.prompt_tokens.unwrap_or(0),
            completion_tokens: self.completion_tokens.unwrap_or(0),
            total_tokens: self.total_tokens.unwrap_or(0),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatChoiceMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<RawToolCall>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RawToolCall {
    id: String,
    #[serde(default)]
    function: RawFunction,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct RawFunction {
    #[serde(default)]
    name: String,
    #[serde(default)]
    arguments: String,
}

fn parse_completion(choice: ChatChoice, usage: Option<ResponseUsage>) -> Completion {
    let content = choice.message.content.unwrap_or_default();
    let tool_calls = choice
        .message
        .tool_calls
        .into_iter()
        .map(|c| {
            let args: Value = if c.function.arguments.trim().is_empty() {
                Value::Object(Default::default())
            } else {
                serde_json::from_str(&c.function.arguments)
                    .unwrap_or_else(|_| Value::String(c.function.arguments.clone()))
            };
            ToolCall {
                id: c.id,
                name: c.function.name,
                arguments: args,
            }
        })
        .collect();
    Completion {
        content,
        tool_calls,
        finish_reason: choice.finish_reason,
        usage: usage.map(|u| u.to_token_usage()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_text_choice() {
        let raw = r#"{"choices":[{"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}"#;
        let parsed: ChatResponse = serde_json::from_str(raw).unwrap();
        let c = parse_completion(parsed.choices.into_iter().next().unwrap(), parsed.usage);
        assert_eq!(c.content, "hi");
        assert!(c.tool_calls.is_empty());
        assert_eq!(c.finish_reason.as_deref(), Some("stop"));
        assert!(c.usage.is_none());
    }

    #[test]
    fn parses_tool_call_choice() {
        let raw = r#"{
            "choices":[{
                "message":{
                    "role":"assistant",
                    "content":null,
                    "tool_calls":[{
                        "id":"call_1",
                        "type":"function",
                        "function":{"name":"read_file","arguments":"{\"path\":\"src/lib.rs\"}"}
                    }]
                },
                "finish_reason":"tool_calls"
            }]
        }"#;
        let parsed: ChatResponse = serde_json::from_str(raw).unwrap();
        let c = parse_completion(parsed.choices.into_iter().next().unwrap(), parsed.usage);
        assert_eq!(c.tool_calls.len(), 1);
        assert_eq!(c.tool_calls[0].name, "read_file");
        assert_eq!(c.tool_calls[0].arguments["path"], "src/lib.rs");
    }

    #[test]
    fn parses_usage_from_response() {
        let raw = r#"{
            "choices":[{"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":41,"completion_tokens":7,"total_tokens":48}
        }"#;
        let parsed: ChatResponse = serde_json::from_str(raw).unwrap();
        let c = parse_completion(parsed.choices.into_iter().next().unwrap(), parsed.usage);
        assert!(c.usage.is_some());
        let u = c.usage.unwrap();
        assert_eq!(u.prompt_tokens, 41);
        assert_eq!(u.completion_tokens, 7);
        assert_eq!(u.total_tokens, 48);
    }

    #[test]
    fn parses_missing_usage_as_none() {
        let raw = r#"{"choices":[{"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}"#;
        let parsed: ChatResponse = serde_json::from_str(raw).unwrap();
        let c = parse_completion(parsed.choices.into_iter().next().unwrap(), parsed.usage);
        assert!(c.usage.is_none());
    }

    #[test]
    fn parses_partial_usage_fills_zeros() {
        let raw = r#"{
            "choices":[{"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
            "usage":{"total_tokens":50}
        }"#;
        let parsed: ChatResponse = serde_json::from_str(raw).unwrap();
        let c = parse_completion(parsed.choices.into_iter().next().unwrap(), parsed.usage);
        assert!(c.usage.is_some());
        let u = c.usage.unwrap();
        assert_eq!(u.prompt_tokens, 0);
        assert_eq!(u.completion_tokens, 0);
        assert_eq!(u.total_tokens, 50);
    }

    #[test]
    fn serialises_assistant_with_tool_calls() {
        let msg = Message::assistant_with_tool_calls(
            "",
            vec![ToolCall {
                id: "abc".into(),
                name: "write_file".into(),
                arguments: serde_json::json!({"path":"a","contents":"b"}),
            }],
        );
        let v = serialize_message(&msg);
        assert_eq!(v["tool_calls"][0]["function"]["name"], "write_file");
        let args = v["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap();
        let decoded: Value = serde_json::from_str(args).unwrap();
        assert_eq!(decoded["contents"], "b");
    }

    #[test]
    fn builds_request_without_tools_omits_field() {
        let req = build_request("m", 0.2, &[Message::user("hi")], &[]);
        assert!(req.get("tools").is_none());
        assert_eq!(req["messages"][0]["role"], "user");
    }

    #[test]
    fn policy_retries_5xx_with_exponential_backoff() {
        let policy = RetryPolicy::default(); // max_retries = 2
                                             // Attempt 0: should return initial_backoff (1s)
        assert_eq!(
            policy.backoff_for(0, Some(503), false),
            Some(Duration::from_secs(1))
        );
        // Attempt 1: should return 2s (1s * 2^1)
        assert_eq!(
            policy.backoff_for(1, Some(500), false),
            Some(Duration::from_secs(2))
        );
        // Attempt 2: should return None (exceeds max_retries=2)
        assert_eq!(policy.backoff_for(2, Some(500), false), None);
    }

    #[test]
    fn policy_retries_network_errors() {
        let policy = RetryPolicy::default();
        // Network error at attempt 0 should return initial_backoff
        assert_eq!(
            policy.backoff_for(0, None, true),
            Some(Duration::from_secs(1))
        );
    }

    #[test]
    fn policy_does_not_retry_4xx() {
        let policy = RetryPolicy::default();
        // 4xx errors should not be retried
        assert_eq!(policy.backoff_for(0, Some(400), false), None);
        assert_eq!(policy.backoff_for(0, Some(401), false), None);
        assert_eq!(policy.backoff_for(0, Some(404), false), None);
        assert_eq!(policy.backoff_for(0, Some(429), false), None);
    }

    #[test]
    fn policy_caps_backoff_at_max() {
        let policy = RetryPolicy {
            max_retries: 10,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(3),
        };
        // At attempt 5, exponential backoff would be 32s but capped to 3s
        assert_eq!(
            policy.backoff_for(5, Some(500), false),
            Some(Duration::from_secs(3))
        );
    }
}
