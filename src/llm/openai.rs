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

use super::StructuredRequest;
use super::{Completion, LlmProvider, StreamSender, TokenUsage, ToolCall, ToolSpec};
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
    max_tokens: u32,
    retry: RetryPolicy,
    stream_tx: Option<StreamSender>,
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
            // DeepSeek defaults to a per-response cap of 4096 tokens; any
            // tool call whose `arguments` string holds more than that — e.g.
            // a `write_file` with a multi-kilobyte `contents` field — gets
            // truncated server-side and arrives as malformed JSON. 16384 is
            // both within DeepSeek's hard ceiling (8192 for v3, 32K-64K for
            // newer models) and big enough for whole-file writes. Callers
            // can override with `with_max_tokens` if their provider supports
            // more or needs less.
            max_tokens: 16384,
            retry: RetryPolicy::default(),
            stream_tx: None,
        }
    }

    /// Enable streaming by providing a channel sender for partial tokens.
    pub fn with_stream_tx(mut self, tx: StreamSender) -> Self {
        self.stream_tx = Some(tx);
        self
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
impl LlmProvider for OpenAiProvider {
    #[tracing::instrument(skip(self, messages, tools), fields(
        provider = %self.base_url.split('/').next_back().unwrap_or("unknown"),
        model = %self.model
    ))]
    async fn complete(&self, messages: &[Message], tools: &[ToolSpec]) -> Result<Completion> {
        let body = build_request(
            &self.model,
            self.temperature,
            self.max_tokens,
            messages,
            tools,
        );
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
                            self.make_err(format!("failed to parse response: {e}; body: {text}"))
                        })?;
                        let choice = parsed
                            .choices
                            .into_iter()
                            .next()
                            .ok_or_else(|| self.make_err("response had no choices"))?;
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

    async fn complete_structured(&self, req: StructuredRequest) -> Result<Value> {
        let mut body = build_request(
            &self.model,
            self.temperature,
            self.max_tokens,
            &req.messages,
            &[],
        );
        body["response_format"] = serde_json::json!({
            "type": "json_schema",
            "json_schema": {
                "name": req.schema_name,
                "strict": true,
                "schema": req.schema,
            }
        });

        let url = format!("{}/chat/completions", self.base_url);

        let mut attempt = 0;
        loop {
            tracing::debug!(target: "recursive::llm", request = %body, "POST {} (structured)", url);
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
                            self.make_err(format!("failed to parse response: {e}; body: {text}"))
                        })?;
                        let choice = parsed
                            .choices
                            .into_iter()
                            .next()
                            .ok_or_else(|| self.make_err("response had no choices"))?;
                        let completion = parse_completion(choice, parsed.usage);
                        // Parse the content as JSON
                        if completion.content.trim().is_empty() {
                            return Err(
                                self.make_err("structured response had empty content".to_string())
                            );
                        }
                        let parsed_json: Value = serde_json::from_str(&completion.content)
                            .map_err(|e| {
                                self.make_err(format!(
                                    "failed to parse structured response as JSON: {e}; content: {}",
                                    completion.content
                                ))
                            })?;
                        return Ok(parsed_json);
                    }

                    let text = resp.text().await?;
                    tracing::debug!(target: "recursive::llm", body = %text, "error response (structured)");

                    if let Some(backoff) =
                        self.retry
                            .backoff_for(attempt, Some(status.as_u16()), is_network_error)
                    {
                        tracing::warn!(
                            target: "recursive::llm",
                            attempt,
                            backoff_ms = backoff.as_millis(),
                            status = status.as_u16(),
                            "transient HTTP error, retrying (structured)"
                        );
                        tokio::time::sleep(backoff).await;
                        attempt += 1;
                        continue;
                    }

                    return Err(self.make_err(format!("HTTP {}: {}", status, text)));
                }
                Err(e) => {
                    if let Some(backoff) = self.retry.backoff_for(attempt, None, true) {
                        tracing::warn!(
                            target: "recursive::llm",
                            attempt,
                            backoff_ms = backoff.as_millis(),
                            error = %e,
                            "network error, retrying (structured)"
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

    async fn stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        stream_tx: Option<StreamSender>,
    ) -> Result<Completion> {
        // If no stream_tx provided, fall back to the instance-level one
        let tx = stream_tx.or_else(|| self.stream_tx.clone());
        self.stream_inner(messages, tools, tx).await
    }
}

impl OpenAiProvider {
    async fn stream_inner(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        stream_tx: Option<StreamSender>,
    ) -> Result<Completion> {
        let mut body = build_request(
            &self.model,
            self.temperature,
            self.max_tokens,
            messages,
            tools,
        );
        body["stream"] = Value::Bool(true);

        let url = format!("{}/chat/completions", self.base_url);

        let mut attempt = 0;
        loop {
            tracing::debug!(target: "recursive::llm", request = %body, "POST {} (stream)", url);
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
                    if status.is_success() {
                        return self.parse_sse_stream(resp, stream_tx.clone()).await;
                    }

                    // Non-2xx: read body and check retry
                    let text = resp.text().await?;
                    tracing::debug!(target: "recursive::llm", body = %text, "error response (stream)");

                    if let Some(backoff) =
                        self.retry
                            .backoff_for(attempt, Some(status.as_u16()), false)
                    {
                        tracing::warn!(
                            target: "recursive::llm",
                            attempt,
                            backoff_ms = backoff.as_millis(),
                            status = status.as_u16(),
                            "transient HTTP error, retrying (stream)"
                        );
                        tokio::time::sleep(backoff).await;
                        attempt += 1;
                        continue;
                    }

                    return Err(self.make_err(format!("HTTP {}: {}", status, text)));
                }
                Err(e) => {
                    if let Some(backoff) = self.retry.backoff_for(attempt, None, true) {
                        tracing::warn!(
                            target: "recursive::llm",
                            attempt,
                            backoff_ms = backoff.as_millis(),
                            error = %e,
                            "network error, retrying (stream)"
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

    /// Parse an SSE stream from a successful HTTP response.
    ///
    /// Reads `data: {...}\n\n` chunks line-by-line, extracts
    /// `choices[0].delta.content` deltas, accumulates them, and emits
    /// each delta through `stream_tx` if configured. Returns the final
    /// `Completion` matching the non-streaming shape.
    async fn parse_sse_stream(
        &self,
        resp: reqwest::Response,
        stream_tx: Option<StreamSender>,
    ) -> Result<Completion> {
        let mut content = String::new();
        let tool_calls: Vec<ToolCall> = Vec::new();
        let mut finish_reason: Option<String> = None;
        let mut usage: Option<TokenUsage> = None;

        // Read the byte stream line by line
        let reader = resp.text().await?;
        for line in reader.lines() {
            if let Some(data) = line.strip_prefix("data: ") {
                // Skip the final "[DONE]" marker
                if data.trim() == "[DONE]" {
                    break;
                }

                // Parse the JSON chunk
                let chunk: Value = serde_json::from_str(data)
                    .map_err(|e| self.make_err(format!("SSE parse error: {e}; data: {data}")))?;

                // Extract delta content
                if let Some(choices) = chunk.get("choices").and_then(|c| c.as_array()) {
                    if let Some(choice) = choices.first() {
                        // Delta content
                        if let Some(delta) = choice.get("delta") {
                            if let Some(delta_content) =
                                delta.get("content").and_then(|c| c.as_str())
                            {
                                if !delta_content.is_empty() {
                                    content.push_str(delta_content);
                                    if let Some(ref tx) = stream_tx {
                                        let _ = tx.send(delta_content.to_string());
                                    }
                                }
                            }
                        }

                        // Finish reason (only on the last chunk)
                        if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                            if !fr.is_empty() {
                                finish_reason = Some(fr.to_string());
                            }
                        }
                    }
                }

                // Usage (only on the last chunk for some providers)
                if let Some(u) = chunk.get("usage") {
                    let prompt =
                        u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                    let completion = u
                        .get("completion_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                    let total = u.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                    let cache_hit = u
                        .get("prompt_cache_hit_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                    let cache_miss = u
                        .get("prompt_cache_miss_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                    usage = Some(TokenUsage {
                        prompt_tokens: prompt,
                        completion_tokens: completion,
                        total_tokens: total,
                        cache_hit_tokens: cache_hit,
                        cache_miss_tokens: cache_miss,
                    });
                }
            }
        }

        Ok(Completion {
            content,
            tool_calls,
            finish_reason,
            usage,
        })
    }
}

fn build_request(
    model: &str,
    temperature: f64,
    max_tokens: u32,
    messages: &[Message],
    tools: &[ToolSpec],
) -> Value {
    let mut req = serde_json::json!({
        "model": model,
        "temperature": temperature,
        "max_tokens": max_tokens,
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
    #[serde(default)]
    prompt_cache_hit_tokens: Option<u32>,
    #[serde(default)]
    prompt_cache_miss_tokens: Option<u32>,
}

impl ResponseUsage {
    fn to_token_usage(&self) -> TokenUsage {
        TokenUsage {
            prompt_tokens: self.prompt_tokens.unwrap_or(0),
            completion_tokens: self.completion_tokens.unwrap_or(0),
            total_tokens: self.total_tokens.unwrap_or(0),
            cache_hit_tokens: self.prompt_cache_hit_tokens.unwrap_or(0),
            cache_miss_tokens: self.prompt_cache_miss_tokens.unwrap_or(0),
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
        assert_eq!(u.cache_hit_tokens, 0);
        assert_eq!(u.cache_miss_tokens, 0);
    }

    #[test]
    fn parses_cache_fields_from_deepseek_usage() {
        let raw = r#"{
            "choices":[{"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
            "usage":{
                "prompt_tokens":100,
                "completion_tokens":50,
                "total_tokens":150,
                "prompt_cache_hit_tokens":60,
                "prompt_cache_miss_tokens":40
            }
        }"#;
        let parsed: ChatResponse = serde_json::from_str(raw).unwrap();
        let c = parse_completion(parsed.choices.into_iter().next().unwrap(), parsed.usage);
        assert!(c.usage.is_some());
        let u = c.usage.unwrap();
        assert_eq!(u.prompt_tokens, 100);
        assert_eq!(u.completion_tokens, 50);
        assert_eq!(u.total_tokens, 150);
        assert_eq!(u.cache_hit_tokens, 60);
        assert_eq!(u.cache_miss_tokens, 40);
    }

    #[test]
    fn parses_cache_fields_as_zero_when_absent() {
        let raw = r#"{
            "choices":[{"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}
        }"#;
        let parsed: ChatResponse = serde_json::from_str(raw).unwrap();
        let c = parse_completion(parsed.choices.into_iter().next().unwrap(), parsed.usage);
        assert!(c.usage.is_some());
        let u = c.usage.unwrap();
        assert_eq!(u.cache_hit_tokens, 0);
        assert_eq!(u.cache_miss_tokens, 0);
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
        let req = build_request("m", 0.2, 16384, &[Message::user("hi")], &[]);
        assert!(req.get("tools").is_none());
        assert_eq!(req["messages"][0]["role"], "user");
        assert_eq!(req["max_tokens"], 16384);
    }

    #[test]
    fn builds_request_includes_max_tokens() {
        let req = build_request("m", 0.2, 1024, &[Message::user("hi")], &[]);
        assert_eq!(req["max_tokens"], 1024);
    }

    #[tokio::test]
    async fn error_includes_model_name_on_network_failure() {
        // Bind a listener on an ephemeral port, then drop it so the port is freed.
        // The next connect attempt gets ECONNREFUSED (a network error).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        // Small sleep so the OS releases the port.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let provider = OpenAiProvider::new(format!("http://{addr}"), "sk-noop", "test-model");
        let err = provider
            .complete(&[Message::user("hi")], &[])
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("model=test-model"),
            "error should contain model name: {msg}"
        );
    }

    #[tokio::test]
    async fn error_includes_model_name_and_status_on_http_error() {
        // Spawn a one-shot TCP listener that sends a 400 response.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        // Use a dedicated thread with a blocking server to avoid tokio issues.
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::{Read, Write};
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            write!(
                stream,
                "HTTP/1.1 400 Bad Request\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
            )
            .unwrap();
            stream.flush().unwrap();
        });

        // Give the server a moment to start
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let provider = OpenAiProvider::new(format!("http://{addr}"), "sk-noop", "test-model-http");
        let err = provider
            .complete(&[Message::user("hi")], &[])
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("model=test-model-http"),
            "error should contain model name: {msg}"
        );
        assert!(
            msg.contains("400"),
            "error should contain HTTP status: {msg}"
        );
    }

    #[tokio::test]
    async fn stream_concatenates_sse_chunks() {
        // Spawn a one-shot TCP server that serves a canned SSE response
        // with 3 chunks. Assert the returned Completion's content is the
        // concatenation of the deltas.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::{Read, Write};
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\" \"},\"finish_reason\":null}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\"World\"},\"finish_reason\":\"stop\"}]}\n\n\
data: [DONE]\n\n";
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body,
            )
            .unwrap();
            stream.flush().unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let provider =
            OpenAiProvider::new(format!("http://{addr}"), "sk-noop", "test-stream-model");
        let completion = provider
            .stream(&[Message::user("hi")], &[], None)
            .await
            .unwrap();
        assert_eq!(completion.content, "Hello World");
        assert_eq!(completion.finish_reason.as_deref(), Some("stop"));
    }

    #[tokio::test]
    async fn stream_fallback_delegates_to_complete() {
        // MockProvider doesn't override stream, so it falls back to complete.
        // Verify the fallback path works by using a MockProvider.
        use crate::llm::MockProvider;
        let provider = MockProvider::new(vec![super::Completion {
            content: "fallback works".to_string(),
            tool_calls: vec![],
            finish_reason: Some("stop".to_string()),
            usage: None,
        }]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let completion = provider
            .stream(&[Message::user("hi")], &[], Some(tx))
            .await
            .unwrap();
        assert_eq!(completion.content, "fallback works");
        // Should have received the full content as a single delta
        let delta = rx.recv().await.unwrap();
        assert_eq!(delta, "fallback works");
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

    #[tokio::test]
    async fn openai_structured_includes_schema_in_request_body() {
        // Spawn a mock server that captures the request body and returns
        // a valid JSON response. Assert the request body contains the
        // response_format block with the schema.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let captured_clone = captured.clone();

        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::{Read, Write};
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            // Extract the JSON body (after the blank line)
            if let Some(body_start) = request.find("\r\n\r\n") {
                let body = request[body_start + 4..].trim().to_string();
                *captured_clone.lock().unwrap() = body.clone();
            }
            let response_body = r#"{"choices":[{"message":{"role":"assistant","content":"{\"answer\":42}"},"finish_reason":"stop"}],"usage":null}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body,
            )
            .unwrap();
            stream.flush().unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let provider =
            OpenAiProvider::new(format!("http://{addr}"), "sk-noop", "test-structured-model");

        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "answer": {"type": "integer"}
            },
            "required": ["answer"]
        });

        let req = StructuredRequest {
            messages: vec![Message::user("what is 6 times 7?".to_string())],
            schema: schema.clone(),
            schema_name: "math_answer".to_string(),
        };

        let _ = provider.complete_structured(req).await;

        // Check the captured request body
        let body_str = captured.lock().unwrap().clone();
        let body: serde_json::Value = serde_json::from_str(&body_str).unwrap();

        // Assert response_format is present with the right shape
        let rf = body.get("response_format").unwrap();
        assert_eq!(rf["type"], "json_schema");
        assert_eq!(rf["json_schema"]["name"], "math_answer");
        assert_eq!(rf["json_schema"]["strict"], true);
        assert_eq!(rf["json_schema"]["schema"], schema);
    }

    #[tokio::test]
    async fn openai_structured_parses_response_json() {
        // Spawn a mock server that returns a known JSON response.
        // Assert the parsed value matches.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::{Read, Write};
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            // Return a JSON object with summary and kept_facts
            let response_body = r#"{"choices":[{"message":{"role":"assistant","content":"{\"summary\":\"test\",\"kept_facts\":[\"a\",\"b\"]}"},"finish_reason":"stop"}],"usage":null}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body,
            )
            .unwrap();
            stream.flush().unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let provider =
            OpenAiProvider::new(format!("http://{addr}"), "sk-noop", "test-structured-model");

        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "summary": {"type": "string"},
                "kept_facts": {"type": "array", "items": {"type": "string"}}
            },
            "required": ["summary", "kept_facts"]
        });

        let req = StructuredRequest {
            messages: vec![Message::user("summarize".to_string())],
            schema,
            schema_name: "summary".to_string(),
        };

        let result = provider.complete_structured(req).await;
        assert!(result.is_ok(), "got error: {:?}", result.err());
        let value = result.unwrap();
        assert_eq!(value["summary"], "test");
        assert_eq!(value["kept_facts"][0], "a");
        assert_eq!(value["kept_facts"][1], "b");
    }
}
