//! Anthropic Messages API adapter.
//!
//! Targets the `/v1/messages` endpoint that Anthropic and compatible
//! providers (MiniMax, DeepSeek) speak.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};

use super::search::{KeywordSearchEngine, SpecWithHint, ToolSearchEngine};
use super::{Completion, LlmProvider, RetryPolicy, StreamSender, TokenUsage, ToolCall, ToolSpec};

/// The name of the `ToolSearchTool` we register as a regular tool
/// in the eager list. The model calls it like any other function
/// tool; we recognize it by name and short-circuit the response
/// with `tool_reference` content blocks.
pub(crate) const TOOL_SEARCH_TOOL_NAME: &str = "ToolSearchTool";

/// Beta header required for `defer_loading` and `tool_reference` support.
/// Without this header, Anthropic's API ignores `defer_loading: true` and
/// does not understand `tool_reference` content blocks.
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
    /// Algorithm used to resolve a `ToolSearchTool` query into a
    /// list of deferred tool names. Always non-null (defaults to
    /// `KeywordSearchEngine`).
    search_engine: Arc<dyn ToolSearchEngine>,
    /// Maximum number of ToolSearchTool round-trips per
    /// `complete_with_search` / `stream_with_search` call.
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
            search_engine: Arc::new(KeywordSearchEngine::new()),
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

    /// Replace the default `KeywordSearchEngine` with a custom
    /// implementation. Useful for tests that want to assert on
    /// which tools the search returns.
    pub fn with_search_engine(mut self, engine: Arc<dyn ToolSearchEngine>) -> Self {
        self.search_engine = engine;
        self
    }

    /// Set the maximum number of ToolSearchTool round-trips per
    /// `complete_with_search` / `stream_with_search` call.
    pub fn with_max_search_rounds(mut self, n: usize) -> Self {
        self.max_search_rounds = n;
        self
    }

    /// The `ToolSearchTool` spec — registered as a regular tool in
    /// the eager list. The model calls it like any other function
    /// tool; we recognize the call by `name == "ToolSearchTool"`.
    fn tool_search_spec() -> (ToolSpec, Option<String>) {
        let description = "Fetches full schema definitions for deferred tools so they can be \
            called. Until fetched, only the name is known — there is no parameter schema, so \
            the tool cannot be invoked. Use `select:<tool_name>` for direct selection, or \
            keywords to search."
            .to_string();
        let search_hint = Some("find search discover discover tools lookup".to_string());
        let spec = ToolSpec {
            name: TOOL_SEARCH_TOOL_NAME.to_string(),
            description,
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Query to find deferred tools. Use \"select:<tool_name>\" \
                            for direct selection, or keywords to search."
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum number of results to return (default: 5)",
                        "default": 5
                    }
                },
                "required": ["query"]
            }),
        };
        (spec, search_hint)
    }

    /// The core search-aware loop. Each iteration:
    ///
    /// 1. Partition deferred tools into "discovered" (previously found via
    ///    ToolSearchTool in message history) and "still-deferred".
    /// 2. Discovered tools are promoted into the eager list with their full
    ///    schema so the model can call them directly this round.
    /// 3. Still-deferred tools are NOT sent in the tools array at all.
    ///    Their names are injected as an `<available-deferred-tools>` block
    ///    in the first user message so the model knows they exist.
    /// 4. Build the request with: ToolSearchTool + caller-eager + discovered.
    /// 5. Send and parse.
    /// 6. If the model called ToolSearchTool, resolve the query, store the
    ///    resolved names in the marker message content (JSON array), and
    ///    recurse so the next round can promote them to eager.
    async fn run_search_aware_loop(
        &self,
        messages: &[Message],
        eager_tools: &[SpecWithHint],
        deferred_tools: &[SpecWithHint],
        round: usize,
    ) -> Result<Completion> {
        let (system, msgs) = extract_system_message(messages);
        let msgs = filter_leading_assistant(&msgs);

        // Promote deferred tools that were discovered in earlier ToolSearch rounds.
        let discovered = extract_discovered_tool_names(&msgs);
        let (promoted, still_deferred): (Vec<_>, Vec<_>) = deferred_tools
            .iter()
            .partition(|(spec, _)| discovered.contains(&spec.name));

        // Only inject ToolSearchTool when there are still-deferred tools the
        // model can search for. If everything is either eager or already
        // discovered (promoted), ToolSearchTool serves no purpose.
        let has_searchable_deferred = !still_deferred.is_empty();

        let mut all_eager: Vec<SpecWithHint> =
            Vec::with_capacity(1 + eager_tools.len() + promoted.len());
        if has_searchable_deferred {
            all_eager.push(Self::tool_search_spec());
        }
        all_eager.extend_from_slice(eager_tools);
        all_eager.extend(promoted.into_iter().cloned());

        // Inject still-deferred names as the first user message so the model
        // knows what it can search for, without exposing full schemas.
        let msgs = inject_available_deferred(&msgs, &still_deferred);

        let body = build_request_with_eager_only(
            &self.model,
            self.temperature,
            self.max_tokens,
            system.as_deref(),
            &msgs,
            &all_eager,
        );
        let url = format!("{}/v1/messages", self.base_url);

        let text = self.post_with_retry(&url, &body).await?;
        let parsed: AnthropicResponse = serde_json::from_str(&text)
            .map_err(|e| self.make_err(format!("failed to parse response: {e}; body: {text}")))?;
        let completion = parse_completion(parsed);

        // If the model called ToolSearchTool, resolve and recurse.
        if let Some(search_call) = completion
            .tool_calls
            .iter()
            .find(|c| c.name == TOOL_SEARCH_TOOL_NAME)
        {
            if round >= self.max_search_rounds {
                tracing::warn!(
                    target: "recursive::llm",
                    round,
                    max = self.max_search_rounds,
                    "ToolSearchTool: hit max_search_rounds, returning current completion"
                );
                return Ok(completion);
            }
            let query = search_call
                .arguments
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let max_results = search_call
                .arguments
                .get("max_results")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize);
            let names = self.search_engine.resolve(query, deferred_tools);
            let names: Vec<String> = match max_results {
                Some(n) => names.into_iter().take(n).collect(),
                None => names,
            };

            // Store resolved names as JSON in the marker message content so
            // extract_discovered_tool_names can find them in the next round.
            let names_json = serde_json::to_string(&names).unwrap_or_default();

            let mut next_messages = msgs.clone();
            next_messages.push(Message {
                role: Role::Assistant,
                content: completion.content.clone(),
                tool_calls: completion.tool_calls.clone(),
                tool_call_id: None,
                reasoning_content: completion.reasoning_content.clone(),
            });
            // Marker user message: tool_call_id identifies it as a ToolSearch
            // result; content holds the resolved names as a JSON array string
            // so the next round can extract them via extract_discovered_tool_names.
            next_messages.push(Message {
                role: Role::User,
                content: names_json,
                tool_calls: Vec::new(),
                tool_call_id: Some(search_call.id.clone()),
                reasoning_content: None,
            });

            return Box::pin(self.run_search_aware_loop(
                &next_messages,
                eager_tools,
                deferred_tools,
                round + 1,
            ))
            .await;
        }

        Ok(completion)
    }

    /// POST `body` to `url` with the standard retry policy. Returns
    /// the response body as a string. Used by both
    /// `complete_with_search` (via `run_search_aware_loop`) and
    /// `stream_inner`.
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

    /// Search-aware completion: model sees eager tools + the
    /// `ToolSearchTool`; deferred tools are sent with
    /// `defer_loading: true` and only become callable after the
    /// model asks for them via `ToolSearchTool`.
    async fn complete_with_search(
        &self,
        messages: &[Message],
        eager_tools: &[(ToolSpec, Option<String>)],
        deferred_tools: &[(ToolSpec, Option<String>)],
    ) -> Result<Completion> {
        self.run_search_aware_loop(messages, eager_tools, deferred_tools, 0)
            .await
    }

    /// Search-aware streaming: promotes already-discovered deferred tools,
    /// injects `<available-deferred-tools>` for the rest, and handles
    /// multi-round ToolSearch across streaming calls.
    async fn stream_with_search(
        &self,
        messages: &[Message],
        eager_tools: &[(ToolSpec, Option<String>)],
        deferred_tools: &[(ToolSpec, Option<String>)],
        stream_tx: Option<StreamSender>,
    ) -> Result<Completion> {
        self.run_stream_search_loop(messages, eager_tools, deferred_tools, 0, stream_tx)
            .await
    }
}

impl AnthropicProvider {
    /// Streaming version of the search-aware loop. Mirrors `run_search_aware_loop`
    /// but uses SSE streaming so the user sees tokens as they arrive.
    async fn run_stream_search_loop(
        &self,
        messages: &[Message],
        eager_tools: &[SpecWithHint],
        deferred_tools: &[SpecWithHint],
        round: usize,
        stream_tx: Option<StreamSender>,
    ) -> Result<Completion> {
        let (system, msgs) = extract_system_message(messages);
        let msgs = filter_leading_assistant(&msgs);

        let discovered = extract_discovered_tool_names(&msgs);
        let (promoted, still_deferred): (Vec<_>, Vec<_>) = deferred_tools
            .iter()
            .partition(|(spec, _)| discovered.contains(&spec.name));

        let has_searchable_deferred = !still_deferred.is_empty();
        let mut all_eager: Vec<SpecWithHint> =
            Vec::with_capacity(1 + eager_tools.len() + promoted.len());
        if has_searchable_deferred {
            all_eager.push(Self::tool_search_spec());
        }
        all_eager.extend_from_slice(eager_tools);
        all_eager.extend(promoted.into_iter().cloned());

        let msgs = inject_available_deferred(&msgs, &still_deferred);

        let mut body = build_request_with_eager_only(
            &self.model,
            self.temperature,
            self.max_tokens,
            system.as_deref(),
            &msgs,
            &all_eager,
        );
        body["stream"] = Value::Bool(true);

        let completion = self.stream_with_body(body, stream_tx.clone()).await?;

        if let Some(search_call) = completion
            .tool_calls
            .iter()
            .find(|c| c.name == TOOL_SEARCH_TOOL_NAME)
        {
            if round >= self.max_search_rounds {
                tracing::warn!(
                    target: "recursive::llm",
                    round,
                    max = self.max_search_rounds,
                    "ToolSearchTool (stream): hit max_search_rounds, returning current completion"
                );
                return Ok(completion);
            }
            let query = search_call
                .arguments
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let max_results = search_call
                .arguments
                .get("max_results")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize);
            let names = self.search_engine.resolve(query, deferred_tools);
            let names: Vec<String> = match max_results {
                Some(n) => names.into_iter().take(n).collect(),
                None => names,
            };
            let names_json = serde_json::to_string(&names).unwrap_or_default();

            let mut next_messages = msgs.clone();
            next_messages.push(Message {
                role: Role::Assistant,
                content: completion.content.clone(),
                tool_calls: completion.tool_calls.clone(),
                tool_call_id: None,
                reasoning_content: completion.reasoning_content.clone(),
            });
            next_messages.push(Message {
                role: Role::User,
                content: names_json,
                tool_calls: Vec::new(),
                tool_call_id: Some(search_call.id.clone()),
                reasoning_content: None,
            });

            return Box::pin(self.run_stream_search_loop(
                &next_messages,
                eager_tools,
                deferred_tools,
                round + 1,
                stream_tx,
            ))
            .await;
        }

        Ok(completion)
    }

    /// Send a pre-built request body as a streaming call and return the
    /// accumulated `Completion`. Handles HTTP retry internally.
    async fn stream_with_body(
        &self,
        body: Value,
        stream_tx: Option<StreamSender>,
    ) -> Result<Completion> {
        let url = format!("{}/v1/messages", self.base_url);
        let mut attempt = 0;
        loop {
            tracing::debug!(target: "recursive::llm", request = %body, "POST {} (stream/search)", url);
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
                        return self.parse_sse_stream(resp, stream_tx).await;
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
                            "transient HTTP error, retrying (stream/search)"
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
                            "network error, retrying (stream/search)"
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
                prompt_tokens: prompt,
                completion_tokens: completion,
                total_tokens: prompt.saturating_add(completion),
                cache_hit_tokens: cache_read.unwrap_or(0),
                cache_miss_tokens: cache_creation.unwrap_or(0),
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

/// Build a request body with only eager tools. Used by the search-aware
/// completion path: deferred tools are handled by injecting an
/// `<available-deferred-tools>` block into the messages instead of being
/// sent in the tools array.
fn build_request_with_eager_only(
    model: &str,
    temperature: f64,
    max_tokens: u32,
    system: Option<&str>,
    messages: &[Message],
    eager_tools: &[SpecWithHint],
) -> Value {
    let mut req = serde_json::json!({
        "model": model,
        "max_tokens": max_tokens,
        "temperature": temperature,
    });

    if let Some(sys) = system {
        req["system"] = Value::String(sys.to_string());
    }

    req["messages"] = Value::Array(serialize_messages_anthropic(messages));

    if !eager_tools.is_empty() {
        let tools_json: Vec<Value> = eager_tools
            .iter()
            .map(|(spec, _)| {
                serde_json::json!({
                    "name": spec.name,
                    "description": spec.description,
                    "input_schema": spec.parameters,
                })
            })
            .collect();
        req["tools"] = Value::Array(tools_json);
    }

    req
}

/// Extract the set of tool names that have been discovered via ToolSearchTool
/// in the message history. A tool is "discovered" when a ToolSearch marker
/// message (tool_call_id set, content is a JSON array of names) exists in
/// the transcript. These tools can be promoted to eager (full schema) for the
/// next LLM round.
fn extract_discovered_tool_names(messages: &[Message]) -> std::collections::HashSet<String> {
    let mut discovered = std::collections::HashSet::new();
    for msg in messages {
        if msg.tool_call_id.is_some() && msg.content.starts_with('[') {
            if let Ok(names) = serde_json::from_str::<Vec<String>>(&msg.content) {
                discovered.extend(names);
            }
        }
    }
    discovered
}

/// Inject an `<available-deferred-tools>` block as the first user message
/// so the model knows which deferred tools it can ask for via ToolSearchTool.
/// Returns a new message list with the block prepended (or the original list
/// unchanged if there are no still-deferred tools).
fn inject_available_deferred(
    messages: &[Message],
    still_deferred: &[&SpecWithHint],
) -> Vec<Message> {
    if still_deferred.is_empty() {
        return messages.to_vec();
    }
    let names: Vec<&str> = still_deferred
        .iter()
        .map(|(s, _)| s.name.as_str())
        .collect();
    let block = format!(
        "<available-deferred-tools>\n{}\n</available-deferred-tools>",
        names.join("\n")
    );
    let mut result = Vec::with_capacity(messages.len() + 1);
    result.push(Message::user(block));
    result.extend_from_slice(messages);
    result
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

    #[test]
    fn build_request_with_eager_only_sends_only_eager_tools() {
        let eager = vec![
            (
                ToolSpec {
                    name: "ToolSearchTool".to_string(),
                    description: "search".to_string(),
                    parameters: json!({"type": "object"}),
                },
                None,
            ),
            (
                ToolSpec {
                    name: "Read".to_string(),
                    description: "Read a file".to_string(),
                    parameters: json!({"type": "object"}),
                },
                None,
            ),
        ];
        let body = build_request_with_eager_only(
            "claude-3",
            0.2,
            4096,
            None,
            &[Message::user("hi")],
            &eager,
        );

        let tools = body["tools"].as_array().expect("tools should be an array");
        assert_eq!(tools.len(), 2);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"ToolSearchTool"));
        assert!(names.contains(&"Read"));
        // Every tool has full input_schema — no stubs.
        for t in tools {
            assert!(t.get("defer_loading").is_none());
            assert!(t["input_schema"].is_object());
        }
    }

    #[test]
    fn serialize_messages_encodes_toolsearch_marker_as_tool_references() {
        // ToolSearch marker: tool_call_id + content = JSON array of names.
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
            // Marker: content is JSON array of resolved names.
            Message {
                role: Role::User,
                content: r#"["notebook_edit"]"#.to_string(),
                tool_calls: Vec::new(),
                tool_call_id: Some("call_xyz".to_string()),
                reasoning_content: None,
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

    #[test]
    fn extract_discovered_tool_names_finds_markers() {
        let msgs = vec![
            Message::user("hi"),
            Message {
                role: Role::User,
                content: r#"["WebFetch","NotebookEdit"]"#.to_string(),
                tool_calls: Vec::new(),
                tool_call_id: Some("call_1".to_string()),
                reasoning_content: None,
            },
        ];
        let discovered = extract_discovered_tool_names(&msgs);
        assert!(discovered.contains("WebFetch"));
        assert!(discovered.contains("NotebookEdit"));
        assert_eq!(discovered.len(), 2);
    }

    #[test]
    fn inject_available_deferred_prepends_block() {
        let msgs = vec![Message::user("hi")];
        let deferred_spec = ToolSpec {
            name: "WebFetch".to_string(),
            description: "fetch".to_string(),
            parameters: json!({}),
        };
        let deferred: Vec<SpecWithHint> = vec![(deferred_spec, None)];
        let deferred_refs: Vec<&SpecWithHint> = deferred.iter().collect();
        let result = inject_available_deferred(&msgs, &deferred_refs);
        assert_eq!(result.len(), 2);
        assert!(result[0].content.contains("<available-deferred-tools>"));
        assert!(result[0].content.contains("WebFetch"));
        assert_eq!(result[1].content, "hi");
    }

    #[tokio::test]
    async fn complete_with_search_round_trips_tool_reference() {
        // Mock server that:
        //   - On request 1, returns a `ToolSearchTool` tool_use.
        //   - On request 2, returns a real `notebook_edit` tool_use.
        // The test asserts:
        //   - The final Completion is the `notebook_edit` call.
        //   - The first request's body has ToolSearchTool + Read (eager);
        //     notebook_edit is NOT in the tools array (only in the
        //     <available-deferred-tools> message).
        //   - The second request's tools array contains notebook_edit with
        //     full schema (promoted after discovery) and the tool_result
        //     has tool_reference content blocks.
        use std::io::{Read, Write};
        use std::sync::{Arc, Mutex};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        let captured_clone = captured.clone();
        std::thread::spawn(move || {
            for i in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 16384];
                let mut acc = Vec::new();
                // Read headers + body. For a small test request,
                // the first read should grab everything.
                loop {
                    let n = stream.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    acc.extend_from_slice(&buf[..n]);
                    if acc.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&acc).to_string();
                let body = request
                    .split("\r\n\r\n")
                    .nth(1)
                    .unwrap_or("")
                    .trim()
                    .to_string();
                captured_clone.lock().unwrap().push(body);

                let response_body = if i == 0 {
                    // First request: model calls ToolSearchTool
                    // looking for the deferred `notebook_edit`.
                    r#"{"type":"message","content":[{"type":"tool_use","id":"call_search_1","name":"ToolSearchTool","input":{"query":"jupyter","max_results":3}}],"stop_reason":"tool_use","usage":{"input_tokens":50,"output_tokens":10}}"#
                } else {
                    // Second request: model now calls the
                    // resolved deferred tool.
                    r#"{"type":"message","content":[{"type":"tool_use","id":"call_real_1","name":"notebook_edit","input":{"path":"analysis.ipynb"}}],"stop_reason":"tool_use","usage":{"input_tokens":80,"output_tokens":20}}"#
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body,
                );
                stream.write_all(response.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let provider =
            AnthropicProvider::new(format!("http://{addr}"), "sk-noop", "claude-3-sonnet").unwrap();

        // Pretend "Read" is eager (always available) and
        // "notebook_edit" is deferred (needs ToolSearchTool).
        let eager = vec![(
            ToolSpec {
                name: "Read".to_string(),
                description: "Read a UTF-8 text file under the workspace.".to_string(),
                parameters: json!({"type": "object"}),
            },
            None,
        )];
        let deferred = vec![(
            ToolSpec {
                name: "notebook_edit".to_string(),
                description: "Edit a Jupyter notebook cell.".to_string(),
                parameters: json!({"type": "object"}),
            },
            Some("jupyter notebook".to_string()),
        )];

        let result = provider
            .complete_with_search(&[Message::user("find and read a file")], &eager, &deferred)
            .await;

        let captured_bodies = captured.lock().unwrap().clone();
        assert_eq!(
            captured_bodies.len(),
            2,
            "expected two requests, got {}",
            captured_bodies.len()
        );

        // First request: only ToolSearchTool + Read in tools array.
        // notebook_edit must NOT be in tools — it's deferred and appears
        // only in the <available-deferred-tools> message.
        let body1: serde_json::Value = serde_json::from_str(&captured_bodies[0]).unwrap();
        let tools1 = body1["tools"].as_array().expect("tools should be array");
        let tool_names: Vec<&str> = tools1
            .iter()
            .map(|t| t["name"].as_str().unwrap_or(""))
            .collect();
        assert!(
            tool_names.contains(&"ToolSearchTool"),
            "missing ToolSearchTool in {:?}",
            tool_names
        );
        assert!(
            tool_names.contains(&"Read"),
            "missing Read in {:?}",
            tool_names
        );
        assert!(
            !tool_names.contains(&"notebook_edit"),
            "notebook_edit should NOT be in tools array on round 0: {:?}",
            tool_names
        );
        // The deferred tool list should be in the first user message.
        let msgs1 = body1["messages"].as_array().unwrap();
        let first_user = msgs1
            .iter()
            .find(|m| m["role"] == "user")
            .expect("should have a user message");
        let first_content = first_user["content"].as_str().unwrap_or("");
        assert!(
            first_content.contains("available-deferred-tools"),
            "first user message should contain <available-deferred-tools>: {:?}",
            first_content
        );
        assert!(
            first_content.contains("notebook_edit"),
            "deferred list should name notebook_edit: {:?}",
            first_content
        );

        // Second request: notebook_edit should now be in the tools array
        // with full schema (promoted after discovery via ToolSearch).
        let body2: serde_json::Value = serde_json::from_str(&captured_bodies[1]).unwrap();
        let tools2 = body2["tools"].as_array().expect("tools2 should be array");
        let tool_names2: Vec<&str> = tools2
            .iter()
            .map(|t| t["name"].as_str().unwrap_or(""))
            .collect();
        assert!(
            tool_names2.contains(&"notebook_edit"),
            "notebook_edit should be promoted to tools array on round 1: {:?}",
            tool_names2
        );
        // Promoted tool must have its real (non-empty) schema.
        let nb = tools2
            .iter()
            .find(|t| t["name"] == "notebook_edit")
            .unwrap();
        assert!(
            nb.get("defer_loading").is_none(),
            "promoted tool should not have defer_loading: {:?}",
            nb
        );
        // The tool_result message should carry tool_reference blocks.
        let msgs2 = body2["messages"].as_array().unwrap();
        let last = &msgs2[msgs2.len() - 1];
        assert_eq!(last["role"], "user");
        let content = last["content"].as_array().expect("content should be array");
        let tool_result = content
            .iter()
            .find(|c| c["type"] == "tool_result")
            .expect("expected a tool_result block");
        assert_eq!(tool_result["tool_use_id"], "call_search_1");
        let refs = tool_result["content"]
            .as_array()
            .expect("tool_result.content should be an array");
        assert!(!refs.is_empty(), "tool_reference array should be non-empty");
        for r in refs {
            assert_eq!(r["type"], "tool_reference");
            assert!(r["tool_name"].is_string());
        }

        // Final completion should be the notebook_edit call
        // (the deferred tool that the model asked to resolve).
        let completion = result.expect("should succeed");
        assert_eq!(completion.tool_calls.len(), 1);
        let tc = &completion.tool_calls[0];
        assert_eq!(tc.name, "notebook_edit");
        assert_eq!(tc.arguments["path"], "analysis.ipynb");
    }
}
