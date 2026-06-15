//! Anthropic Messages API adapter.
//!
//! Targets the `/v1/messages` endpoint that Anthropic and compatible
//! providers (MiniMax, DeepSeek) speak.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;

use super::{Completion, LlmProvider, RetryPolicy, StreamSender, TokenUsage, ToolCall, ToolSpec};

/// Beta header required for `tool_reference` block support in the
/// Anthropic Messages API.
const TOOL_SEARCH_BETA_HEADER: &str = "advanced-tool-use-2025-11-20";

use crate::error::{Error, Result};
use crate::message::{Message, Role};

#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    base_url: String,
    api_key: String,
    model: String,
    client: Client,
    temperature: f64,
    max_tokens: u32,
    retry: RetryPolicy,
    /// Cap on `ToolSearchTool` round-trips per `complete_with_search` /
    /// `stream_with_search` call, mirroring the OpenAI provider. Anthropic
    /// itself does not yet drive a server-side tool-search loop, so this
    /// field is configured (and surfaced via `with_max_search_rounds`) for
    /// API parity with `OpenAiProvider` but is currently unused at
    /// runtime. Wire it up when Anthropic gets its own deferred-tool
    /// search loop.
    #[allow(dead_code)]
    max_search_rounds: usize,
}

impl AnthropicProvider {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(180))
            .build()
            .map_err(|e| Error::Config {
                message: format!("failed to build HTTP client: {e}"),
            })?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
            client,
            temperature: 0.2,
            max_tokens: 4096,
            retry: RetryPolicy::default(),
            max_search_rounds: 3,
        })
    }

    /// Build an `Error::Llm` with the model name prefixed.
    fn make_err(&self, ctx: impl Into<String>) -> Error {
        Error::Llm {
            provider: self.model.clone(),
            message: ctx.into(),
        }
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

    /// Cap on `ToolSearchTool` round-trips per `complete_with_search` /
    /// `stream_with_search` call. Mirrors `OpenAiProvider`; the value is
    /// stored but not yet consumed by the Anthropic code path (see the
    /// field doc for context).
    pub fn with_max_search_rounds(mut self, n: usize) -> Self {
        self.max_search_rounds = n;
        self
    }

    /// POST `body` to `url` with the standard retry policy.
    async fn post_with_retry(&self, url: &str, body: &Value) -> Result<String> {
        let mut attempt = 0;
        loop {
            tracing::debug!(target: "recursive::llm", request = %body, "POST {}", url);
            let result = self
                .client
                .post(url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .header("anthropic-beta", TOOL_SEARCH_BETA_HEADER)
                .header("content-type", "application/json")
                .json(body)
                .send()
                .await;
            match result {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return resp.text().await.map_err(Error::from);
                    }
                    let text = resp.text().await?;
                    if let Some(backoff) =
                        self.retry
                            .backoff_for(attempt, Some(status.as_u16()), false)
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
                    return Err(self.make_err(format!("HTTP {}: {}", status, text)));
                }
                Err(e) => {
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

#[async_trait]
impl LlmProvider for AnthropicProvider {
    /// Whether to use deferred tool loading via `tool_reference` blocks.
    ///
    /// `tool_reference` is an Anthropic beta feature (`advanced-tool-use-2025-11-20`).
    /// The official `api.anthropic.com` endpoint supports it; third-party
    /// Anthropic-compatible endpoints (DeepSeek, MiniMax, etc.) typically do not.
    /// Mirroring Claude Code's `isFirstPartyAnthropicBaseUrl()` check: only enable
    /// deferred tools when the endpoint is the official Anthropic API.
    ///
    /// Users on compatible proxies can set `RECURSIVE_DEFERRED_TOOLS=true` to opt in.
    fn supports_deferred_tools(&self) -> bool {
        if std::env::var("RECURSIVE_DEFERRED_TOOLS")
            .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
            .unwrap_or(false)
        {
            return true;
        }
        self.base_url.contains("api.anthropic.com")
    }

    async fn complete(&self, messages: &[Message], tools: &[ToolSpec]) -> Result<Completion> {
        let (system, messages) = extract_system_message(messages);
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
        let text = self.post_with_retry(&url, &body).await?;
        let parsed: AnthropicResponse = serde_json::from_str(&text)
            .map_err(|e| self.make_err(format!("failed to parse response: {e}; body: {text}")))?;
        Ok(parse_completion(parsed))
    }

    async fn stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        stream_tx: Option<StreamSender>,
    ) -> Result<Completion> {
        self.stream_inner(messages, tools, stream_tx).await
    }
}

impl AnthropicProvider {
    /// Send a pre-built request body as a streaming call and return the
    /// accumulated `Completion`. Handles HTTP retry internally.
    /// Internal streaming implementation.
    async fn stream_inner(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        stream_tx: Option<StreamSender>,
    ) -> Result<Completion> {
        let (system, messages) = extract_system_message(messages);
        let messages = filter_leading_assistant(&messages);

        let mut body = build_request(
            &self.model,
            self.temperature,
            self.max_tokens,
            system.as_deref(),
            &messages,
            tools,
        );
        body["stream"] = Value::Bool(true);

        let url = format!("{}/v1/messages", self.base_url);

        let mut attempt = 0;
        loop {
            tracing::debug!(target: "recursive::llm", request = %body, "POST {} (stream)", url);
            let result = self
                .client
                .post(&url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .header("anthropic-beta", TOOL_SEARCH_BETA_HEADER)
                .header("content-type", "application/json")
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

    /// Parse an Anthropic SSE stream from a successful HTTP response.
    ///
    /// Anthropic SSE format:
    ///   event: message_start\n
    ///   data: {"type":"message_start","message":{...}}\n\n
    ///   event: content_block_start\n
    ///   data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Hello"}}\n\n
    ///   event: content_block_delta\n
    ///   data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" World"}}\n\n
    ///   event: message_delta\n
    ///   data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}\n\n
    ///   event: message_stop\n
    ///   data: {"type":"message_stop"}\n\n
    async fn parse_sse_stream(
        &self,
        resp: reqwest::Response,
        stream_tx: Option<StreamSender>,
    ) -> Result<Completion> {
        let mut content = String::new();
        let mut tool_calls: Vec<StreamToolCall> = Vec::new();
        let mut finish_reason: Option<String> = None;
        let mut usage: Option<TokenUsage> = None;
        let mut input_tokens: Option<u32> = None;
        let mut output_tokens: Option<u32> = None;
        let mut cache_creation: Option<u32> = None;
        let mut cache_read: Option<u32> = None;

        // Read the full response body as text and parse line by line
        let reader = resp.text().await?;
        let mut current_event: Option<String> = None;

        for line in reader.lines() {
            if let Some(event_name) = line.strip_prefix("event: ") {
                current_event = Some(event_name.to_string());
                continue;
            }

            if let Some(data) = line.strip_prefix("data: ") {
                let event_type = current_event.as_deref().unwrap_or("unknown");

                match event_type {
                    "message_start" => {
                        // Extract input tokens from the message
                        let parsed: Value = serde_json::from_str(data).map_err(|e| {
                            self.make_err(format!(
                                "SSE parse error (message_start): {e}; data: {data}"
                            ))
                        })?;
                        if let Some(msg) = parsed.get("message") {
                            if let Some(u) = msg.get("usage") {
                                input_tokens = u
                                    .get("input_tokens")
                                    .and_then(|v| v.as_u64())
                                    .map(|v| v as u32);
                                cache_creation = u
                                    .get("cache_creation_input_tokens")
                                    .and_then(|v| v.as_u64())
                                    .map(|v| v as u32);
                                cache_read = u
                                    .get("cache_read_input_tokens")
                                    .and_then(|v| v.as_u64())
                                    .map(|v| v as u32);
                            }
                        }
                    }
                    "content_block_start" => {
                        let parsed: Value = serde_json::from_str(data).map_err(|e| {
                            self.make_err(format!(
                                "SSE parse error (content_block_start): {e}; data: {data}"
                            ))
                        })?;
                        if let Some(block) = parsed.get("content_block") {
                            let block_type =
                                block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                            let index =
                                parsed.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                            match block_type {
                                "tool_use" => {
                                    let id = block
                                        .get("id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let name = block
                                        .get("name")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    // Ensure the vec is large enough
                                    while tool_calls.len() <= index {
                                        tool_calls.push(StreamToolCall::default());
                                    }
                                    tool_calls[index].id = id;
                                    tool_calls[index].name = name;
                                }
                                "text" => {
                                    // Initial text content (if any)
                                    if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                                        if !text.is_empty() {
                                            content.push_str(text);
                                            if let Some(ref tx) = stream_tx {
                                                let _ = tx.send(text.to_string());
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    "content_block_delta" => {
                        let parsed: Value = serde_json::from_str(data).map_err(|e| {
                            self.make_err(format!(
                                "SSE parse error (content_block_delta): {e}; data: {data}"
                            ))
                        })?;
                        if let Some(delta) = parsed.get("delta") {
                            let delta_type =
                                delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
                            match delta_type {
                                "text_delta" => {
                                    if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                                        if !text.is_empty() {
                                            content.push_str(text);
                                            if let Some(ref tx) = stream_tx {
                                                let _ = tx.send(text.to_string());
                                            }
                                        }
                                    }
                                }
                                "input_json_delta" => {
                                    if let Some(partial) =
                                        delta.get("partial_json").and_then(|v| v.as_str())
                                    {
                                        let index = parsed
                                            .get("index")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0)
                                            as usize;
                                        while tool_calls.len() <= index {
                                            tool_calls.push(StreamToolCall::default());
                                        }
                                        tool_calls[index].partial_json.push_str(partial);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    "message_delta" => {
                        let parsed: Value = serde_json::from_str(data).map_err(|e| {
                            self.make_err(format!(
                                "SSE parse error (message_delta): {e}; data: {data}"
                            ))
                        })?;
                        if let Some(delta) = parsed.get("delta") {
                            if let Some(reason) = delta.get("stop_reason").and_then(|v| v.as_str())
                            {
                                finish_reason = Some(reason.to_string());
                            }
                        }
                        if let Some(u) = parsed.get("usage") {
                            output_tokens = u
                                .get("output_tokens")
                                .and_then(|v| v.as_u64())
                                .map(|v| v as u32);
                        }
                    }
                    "message_stop" => {
                        // End of stream - nothing to extract
                    }
                    "ping" => {
                        // Anthropic sends periodic pings to keep the connection alive
                    }
                    _ => {
                        tracing::debug!(target: "recursive::llm", event = %event_type, data = %data, "unhandled SSE event");
                    }
                }

                current_event = None;
            }
        }

        // Build final TokenUsage from accumulated fields
        if input_tokens.is_some() || output_tokens.is_some() {
            let prompt = input_tokens.unwrap_or(0);
            let completion = output_tokens.unwrap_or(0);
            usage = Some(TokenUsage {
                reasoning_tokens: 0,
                prompt_tokens: prompt,
                completion_tokens: completion,
                total_tokens: prompt.saturating_add(completion),
                cache_hit_tokens: cache_read.unwrap_or(0),
                cache_miss_tokens: cache_creation.unwrap_or(0),
                // Goal 273: not yet reported by Anthropic. Default 0.
            });
        }

        // Convert streamed tool calls to final ToolCall objects
        let final_tool_calls: Vec<ToolCall> = tool_calls
            .into_iter()
            .filter(|tc| !tc.id.is_empty())
            .map(|tc| {
                let args: Value = if tc.partial_json.trim().is_empty() {
                    Value::Object(Default::default())
                } else {
                    serde_json::from_str(&tc.partial_json)
                        .unwrap_or_else(|_| Value::String(tc.partial_json.clone()))
                };
                ToolCall {
                    id: tc.id,
                    name: tc.name,
                    arguments: args,
                }
            })
            .collect();

        Ok(Completion {
            content,
            tool_calls: final_tool_calls,
            finish_reason: finish_reason.map(|r| match r.as_str() {
                "end_turn" => "stop".to_string(),
                "max_tokens" => "length".to_string(),
                "tool_use" => "tool_calls".to_string(),
                other => other.to_string(),
            }),
            usage,
            reasoning_content: None,
        })
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

    let msgs: Vec<Value> = serialize_messages_anthropic(messages);
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

/// Serialize a slice of messages for the Anthropic API, merging consecutive
/// tool-result messages into a single user message with multiple `tool_result`
/// content blocks.
///
/// Anthropic requires that when an assistant message contains multiple `tool_use`
/// blocks, the immediately following user message must contain ALL corresponding
/// `tool_result` blocks in a single message. Sending them as separate messages
/// causes HTTP 400 "tool_use ids were found without tool_result blocks".
///
/// ToolSearch marker messages (tool_call_id set, content is a JSON array of
/// resolved names) are serialized with `tool_reference` content blocks so the
/// Anthropic API understands them as "here are the discovered tool schemas".
fn serialize_messages_anthropic(messages: &[Message]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    let mut i = 0;
    while i < messages.len() {
        let m = &messages[i];
        // Check if this is a tool result message (Role::Tool or has tool_call_id)
        if m.tool_call_id.is_some() || m.role == Role::Tool {
            // Collect all consecutive tool result messages into one user message
            let mut blocks: Vec<Value> = Vec::new();
            while i < messages.len() {
                let tm = &messages[i];
                if tm.tool_call_id.is_none() && tm.role != Role::Tool {
                    break;
                }
                if let Some(id) = &tm.tool_call_id {
                    // ToolSearch markers store resolved names as a JSON array.
                    // Serialize as tool_reference blocks so the API can expand
                    // them into full schemas in the model's context.
                    let content_value = if tm.content.starts_with('[') {
                        if let Ok(names) = serde_json::from_str::<Vec<String>>(&tm.content) {
                            Value::Array(
                                names
                                    .iter()
                                    .map(|n| {
                                        serde_json::json!({
                                            "type": "tool_reference",
                                            "tool_name": n,
                                        })
                                    })
                                    .collect(),
                            )
                        } else {
                            Value::String(tm.content.clone())
                        }
                    } else {
                        Value::String(tm.content.clone())
                    };
                    blocks.push(serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": id,
                        "content": content_value,
                    }));
                }
                i += 1;
            }
            if !blocks.is_empty() {
                out.push(serde_json::json!({
                    "role": "user",
                    "content": blocks,
                }));
            }
        } else {
            out.push(serialize_message(m));
            i += 1;
        }
    }
    out
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
    // Extended thinking blocks (MiniMax-M3, deepseek-v4-flash, etc.) — skip silently.
    #[serde(other)]
    Unknown,
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
            reasoning_tokens: 0,
            prompt_tokens: self.input_tokens.unwrap_or(0),
            completion_tokens: self.output_tokens.unwrap_or(0),
            total_tokens: self
                .input_tokens
                .unwrap_or(0)
                .saturating_add(self.output_tokens.unwrap_or(0)),
            cache_hit_tokens: self.cache_read_input_tokens.unwrap_or(0),
            cache_miss_tokens: self.cache_creation_input_tokens.unwrap_or(0),
            // Goal 273: Anthropic extended-thinking emits a separate
            // `thinking_tokens` field. Default 0 — the field may be
            // added once Anthropic's response shape is finalised.
        }
    }
}

/// Accumulator for tool calls being built from SSE stream events.
#[derive(Default)]
struct StreamToolCall {
    id: String,
    name: String,
    partial_json: String,
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
            ContentBlock::Unknown => {}
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
        reasoning_content: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::TOOL_SEARCH_TOOL_NAME;
    use serde_json::json;

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
                {"type": "tool_use", "id": "call_1", "name": "Read", "input": {"path": "src/lib.rs"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 100, "output_tokens": 50}
        }"#;
        let parsed: AnthropicResponse = serde_json::from_str(raw).unwrap();
        let c = parse_completion(parsed);
        assert!(c.content.is_empty());
        assert_eq!(c.tool_calls.len(), 1);
        assert_eq!(c.tool_calls[0].id, "call_1");
        assert_eq!(c.tool_calls[0].name, "Read");
        assert_eq!(c.tool_calls[0].arguments["path"], "src/lib.rs");
    }

    #[test]
    fn parses_mixed_content_response() {
        let raw = r#"{
            "type": "message",
            "content": [
                {"type": "text", "text": "I'll read that file. "},
                {"type": "tool_use", "id": "call_1", "name": "Read", "input": {"path": "src/lib.rs"}}
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
                name: "Write".to_string(),
                arguments: serde_json::json!({"path": "a", "contents": "b"}),
            }],
        );
        let v = serialize_message(&msg);
        assert_eq!(v["role"], "assistant");
        let content = &v["content"][0];
        assert_eq!(content["type"], "tool_use");
        assert_eq!(content["name"], "Write");
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
            AnthropicProvider::new(format!("http://{addr}"), "sk-noop", "claude-3-sonnet").unwrap();
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
            AnthropicProvider::new(format!("http://{addr}"), "sk-invalid", "claude-3-opus")
                .unwrap();
        let err = provider
            .complete(&[Message::user("hi".to_string())], &[])
            .await
            .unwrap_err();

        let _ = handle.join();

        let msg = err.to_string();
        assert!(
            msg.contains("claude-3-opus"),
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

            let body = r#"{"type":"message","content":[{"type":"tool_use","id":"call_abc123","name":"Read","input":{"path":"src/lib.rs"}}],"stop_reason":"tool_use","usage":{"input_tokens":50,"output_tokens":30}}"#;
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
            AnthropicProvider::new(format!("http://{addr}"), "sk-noop", "claude-3-sonnet").unwrap();
        let result = provider
            .complete(&[Message::user("read the file".to_string())], &[])
            .await;

        let _ = handle.join();

        let completion = result.expect("should succeed");
        assert_eq!(completion.tool_calls.len(), 1);
        let tc = &completion.tool_calls[0];
        assert_eq!(tc.id, "call_abc123");
        assert_eq!(tc.name, "Read");
        assert_eq!(tc.arguments["path"], "src/lib.rs");
    }

    #[tokio::test]
    async fn test_d_stream_request_includes_stream_true() {
        // Spawn a mock server that captures the request body and returns
        // a valid SSE response. Assert the request body contains "stream": true.
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
            if let Some(body_start) = request.find("\r\n\r\n") {
                let body = request[body_start + 4..].trim().to_string();
                *captured_clone.lock().unwrap() = body.clone();
            }
            // Return a valid SSE stream (just message_stop to end quickly)
            let body = "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body,
            ).unwrap();
            stream.flush().unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let provider =
            AnthropicProvider::new(format!("http://{addr}"), "sk-noop", "claude-3-sonnet").unwrap();
        let _ = provider
            .stream(&[Message::user("hi".to_string())], &[], None)
            .await;

        let body_str = captured.lock().unwrap().clone();
        let body: serde_json::Value = serde_json::from_str(&body_str).unwrap();
        assert_eq!(body["stream"], true);
    }

    #[tokio::test]
    async fn test_e_stream_text_deltas_accumulate() {
        // Spawn a mock server that sends text deltas via SSE.
        // Assert the returned Completion's content is the concatenation.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::{Read, Write};
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let body = "\
event: message_start
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-3\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}

event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"Hello\"}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" \"}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"World\"}}

event: message_delta
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}

event: message_stop
data: {\"type\":\"message_stop\"}

";
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body,
            ).unwrap();
            stream.flush().unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let provider =
            AnthropicProvider::new(format!("http://{addr}"), "sk-noop", "claude-3-sonnet").unwrap();
        let completion = provider
            .stream(&[Message::user("hi".to_string())], &[], None)
            .await
            .unwrap();
        assert_eq!(completion.content, "Hello World");
        assert_eq!(completion.finish_reason.as_deref(), Some("stop"));
        assert!(completion.usage.is_some());
        let u = completion.usage.unwrap();
        assert_eq!(u.prompt_tokens, 10);
        assert_eq!(u.completion_tokens, 5);
    }

    #[tokio::test]
    async fn test_f_stream_tool_use_assembles_tool_calls() {
        // Spawn a mock server that sends tool_use blocks via SSE.
        // Assert the returned Completion has the correct tool_calls.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::{Read, Write};
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let body = "\
event: message_start
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-3\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":50,\"output_tokens\":0}}}

event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_abc123\",\"name\":\"Read\",\"input\":{}}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"src/\"}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"lib.rs\\\"}\"}}

event: message_delta
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":30}}

event: message_stop
data: {\"type\":\"message_stop\"}

";
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body,
            ).unwrap();
            stream.flush().unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let provider =
            AnthropicProvider::new(format!("http://{addr}"), "sk-noop", "claude-3-sonnet").unwrap();
        let completion = provider
            .stream(&[Message::user("read the file".to_string())], &[], None)
            .await
            .unwrap();
        assert_eq!(completion.tool_calls.len(), 1);
        let tc = &completion.tool_calls[0];
        assert_eq!(tc.id, "call_abc123");
        assert_eq!(tc.name, "Read");
        assert_eq!(tc.arguments["path"], "src/lib.rs");
        assert_eq!(completion.finish_reason.as_deref(), Some("tool_calls"));
    }

    #[tokio::test]
    async fn test_g_stream_tx_receives_text_chunks() {
        // Spawn a mock server that sends text deltas via SSE.
        // Assert the stream_tx channel receives incremental chunks.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::{Read, Write};
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let body = "\
event: message_start
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-3\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}

event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"A\"}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"B\"}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"C\"}}

event: message_delta
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}

event: message_stop
data: {\"type\":\"message_stop\"}

";
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body,
            ).unwrap();
            stream.flush().unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let provider =
            AnthropicProvider::new(format!("http://{addr}"), "sk-noop", "claude-3-sonnet").unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let completion = provider
            .stream(&[Message::user("hi".to_string())], &[], Some(tx))
            .await
            .unwrap();
        assert_eq!(completion.content, "ABC");

        // Should have received 3 deltas
        let mut deltas = Vec::new();
        while let Some(d) = rx.recv().await {
            deltas.push(d);
            if deltas.len() >= 3 {
                break;
            }
        }
        assert_eq!(deltas, vec!["A", "B", "C"]);
    }

    #[tokio::test]
    async fn test_h_stream_with_end_turn_produces_stop_reason() {
        // Spawn a mock server that sends a complete SSE stream with
        // end_turn stop_reason. Assert the Completion has finish_reason "stop".
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::{Read, Write};
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let body = "\
event: message_start
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-3\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}

event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"Hello\"}}

event: message_delta
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}

event: message_stop
data: {\"type\":\"message_stop\"}

";
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body,
            ).unwrap();
            stream.flush().unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let provider =
            AnthropicProvider::new(format!("http://{addr}"), "sk-noop", "claude-3-sonnet").unwrap();
        let completion = provider
            .stream(&[Message::user("hi".to_string())], &[], None)
            .await
            .unwrap();
        assert_eq!(completion.content, "Hello");
        assert_eq!(completion.finish_reason.as_deref(), Some("stop"));
        assert!(completion.usage.is_some());
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

    // ToolSearchTool is now a regular tool handled by run_core, not by the
    // provider. The provider only needs to:
    //   1. Serialize ToolSearch tool_result as tool_reference blocks.
    //   2. Send whatever specs run_core passes (already filtered to eager).
    // The following tests verify these two responsibilities.

    #[test]
    fn build_request_serializes_passed_specs_verbatim() {
        let specs = vec![
            ToolSpec {
                name: "ToolSearchTool".to_string(),
                description: "search".to_string(),
                parameters: json!({"type": "object"}),
            },
            ToolSpec {
                name: "Read".to_string(),
                description: "Read a file".to_string(),
                parameters: json!({"type": "object"}),
            },
        ];
        let body = build_request("claude-3", 0.2, 4096, None, &[Message::user("hi")], &specs);
        let tools = body["tools"].as_array().expect("tools should be array");
        assert_eq!(tools.len(), 2);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"ToolSearchTool"));
        assert!(names.contains(&"Read"));
        for t in tools {
            assert!(t["input_schema"].is_object());
        }
    }

    #[test]
    fn serialize_messages_encodes_toolsearch_result_as_tool_references() {
        // When a ToolSearchTool result message has content that is a JSON
        // array of tool names, serialize_messages_anthropic must emit
        // tool_reference content blocks so Anthropic can expand them.
        let msgs = vec![
            Message::user("search for foo"),
            Message::assistant_with_tool_calls(
                String::new(),
                vec![ToolCall {
                    id: "call_xyz".to_string(),
                    name: TOOL_SEARCH_TOOL_NAME.to_string(),
                    arguments: json!({"query": "foo"}),
                }],
            ),
            Message {
                role: Role::User,
                content: r#"["notebook_edit"]"#.to_string(),
                tool_calls: Vec::new(),
                tool_call_id: Some("call_xyz".to_string()),
                reasoning_content: None,
                is_compaction_summary: false,
            },
        ];

        let wire_msgs = serialize_messages_anthropic(&msgs);
        assert_eq!(wire_msgs.len(), 3);
        let last = &wire_msgs[2];
        assert_eq!(last["role"], "user");
        let content = last["content"].as_array().expect("content should be array");
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[0]["tool_use_id"], "call_xyz");
        let inner = content[0]["content"]
            .as_array()
            .expect("inner content must be array");
        assert_eq!(inner.len(), 1);
        assert_eq!(inner[0]["type"], "tool_reference");
        assert_eq!(inner[0]["tool_name"], "notebook_edit");
    }

    #[tokio::test]
    async fn complete_sends_specs_and_returns_tool_call() {
        // Verify the provider sends whatever specs it receives and parses
        // the response correctly. The deferred-tool filtering is done by
        // run_core before calling complete(), so the provider sees only
        // eager specs here.
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            let response_body = r#"{"type":"message","content":[{"type":"tool_use","id":"call_1","name":"Read","input":{"path":"foo.txt"}}],"stop_reason":"tool_use","usage":{"input_tokens":50,"output_tokens":10}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body,
            );
            stream.write_all(response.as_bytes()).unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let provider =
            AnthropicProvider::new(format!("http://{addr}"), "sk-noop", "claude-3-sonnet").unwrap();

        let specs = vec![
            ToolSpec {
                name: "ToolSearchTool".to_string(),
                description: "search for deferred tools".to_string(),
                parameters: json!({"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}),
            },
            ToolSpec {
                name: "Read".to_string(),
                description: "Read a file".to_string(),
                parameters: json!({"type": "object"}),
            },
        ];

        let result = provider
            .complete(&[Message::user("read foo.txt")], &specs)
            .await
            .expect("should succeed");

        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "Read");
    }
}
