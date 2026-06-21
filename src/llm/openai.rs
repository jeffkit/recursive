//! OpenAI-compatible chat-completions adapter.
//!
//! Targets the `/chat/completions` shape that OpenAI, Azure (via gateway),
//! GLM (Zhipu), DeepSeek, Moonshot, Together, Ollama and many others speak.
//! The only thing that varies is the base URL + model name + API key, which
//! is all driven by config.
//!
//! ## Deferred tool loading (software-layer ToolSearch)
//!
//! This provider implements the same deferred-tool pattern as `AnthropicProvider`
//! but in pure software — no API-level `defer_loading` or `tool_reference` needed.
//! On each `complete_with_search` / `stream_with_search` call:
//!
//! 1. Only eager tools + `ToolSearchTool` are sent in the initial request.
//! 2. If the model calls `ToolSearchTool`, the query is resolved against the
//!    deferred list, the matched schemas are returned as plain JSON in a
//!    `tool_result` message, and a new request is sent with the matched tools
//!    appended to the eager list.
//! 3. Capped at `max_search_rounds` to prevent infinite loops.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::search::{KeywordSearchEngine, SpecWithHint, ToolSearchEngine};
use super::StructuredRequest;
use super::{
    ChatProvider, Completion, RetryPolicy, StreamChunk, StreamSender, TokenUsage, ToolCall,
    ToolSpec,
};
use crate::error::{Error, Result};
use crate::message::{Message, Role};

const TOOL_SEARCH_TOOL_NAME: &str = "ToolSearchTool";

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
    /// Algorithm used to resolve a `ToolSearchTool` query into a list of
    /// deferred tool names. Defaults to `KeywordSearchEngine`.
    search_engine: Arc<dyn ToolSearchEngine>,
    /// Maximum number of ToolSearchTool round-trips per
    /// `complete_with_search` / `stream_with_search` call.
    max_search_rounds: usize,
}

impl OpenAiProvider {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> crate::error::Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(180))
            .build()
            .map_err(|e| crate::error::Error::Config {
                message: format!("failed to build HTTP client: {e}"),
            })?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
            client,
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
            search_engine: Arc::new(KeywordSearchEngine::new()),
            max_search_rounds: 3,
        })
    }

    /// Replace the search engine used to resolve `ToolSearchTool` queries.
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

    /// Enable streaming by providing a channel sender for partial tokens.
    pub fn with_stream_tx(mut self, tx: StreamSender) -> Self {
        self.stream_tx = Some(tx);
        self
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

    /// POST `body` to `url` with retry, returning the raw response text on success.
    ///
    /// Handles: network errors, 5xx transient errors, and HTTP 200 with empty body
    /// (MiniMax transient failure). All three are retried with exponential back-off
    /// according to `self.retry`. Non-transient 4xx errors are returned immediately.
    async fn post_json_with_retry(&self, url: &str, body: &Value, label: &str) -> Result<String> {
        let mut attempt = 0;
        loop {
            tracing::debug!(target: "recursive::llm", request = %body, "POST {url} ({label})");
            let result = self
                .client
                .post(url)
                .bearer_auth(&self.api_key)
                .json(body)
                .send()
                .await;

            match result {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        let text = resp.text().await?;
                        if text.trim().is_empty() {
                            if let Some(backoff) = self.retry.backoff_for(attempt, None, true) {
                                tracing::warn!(
                                    target: "recursive::llm",
                                    attempt,
                                    backoff_ms = backoff.as_millis(),
                                    "HTTP 200 but empty body ({label}), retrying"
                                );
                                tokio::time::sleep(backoff).await;
                                attempt += 1;
                                continue;
                            }
                            return Err(self.make_err("HTTP 200 but response body is empty"));
                        }
                        return Ok(text);
                    }

                    let text = resp.text().await?;
                    tracing::debug!(target: "recursive::llm", body = %text, "error response ({label})");

                    if let Some(backoff) =
                        self.retry
                            .backoff_for(attempt, Some(status.as_u16()), false)
                    {
                        tracing::warn!(
                            target: "recursive::llm",
                            attempt,
                            backoff_ms = backoff.as_millis(),
                            status = status.as_u16(),
                            "transient HTTP error, retrying ({label})"
                        );
                        tokio::time::sleep(backoff).await;
                        attempt += 1;
                        continue;
                    }

                    return Err(self.make_err(format!("HTTP {status}: {text}")));
                }
                Err(e) => {
                    if let Some(backoff) = self.retry.backoff_for(attempt, None, true) {
                        tracing::warn!(
                            target: "recursive::llm",
                            attempt,
                            backoff_ms = backoff.as_millis(),
                            error = %e,
                            "network error, retrying ({label})"
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
impl ChatProvider for OpenAiProvider {
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
        let text = self.post_json_with_retry(&url, &body, "complete").await?;
        let parsed: ChatResponse = serde_json::from_str(&text)
            .map_err(|e| self.make_err(format!("failed to parse response: {e}; body: {text}")))?;
        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| self.make_err("response had no choices"))?;
        Ok(parse_completion(choice, parsed.usage))
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
        let text = self.post_json_with_retry(&url, &body, "structured").await?;
        let parsed: ChatResponse = serde_json::from_str(&text)
            .map_err(|e| self.make_err(format!("failed to parse response: {e}; body: {text}")))?;
        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| self.make_err("response had no choices"))?;
        let completion = parse_completion(choice, parsed.usage);
        if completion.content.trim().is_empty() {
            return Err(self.make_err("structured response had empty content"));
        }
        serde_json::from_str(&completion.content).map_err(|e| {
            self.make_err(format!(
                "failed to parse structured response as JSON: {e}; content: {}",
                completion.content
            ))
        })
    }

    async fn complete_with_search(
        &self,
        messages: &[Message],
        eager_tools: &[SpecWithHint],
        deferred_tools: &[SpecWithHint],
    ) -> Result<Completion> {
        if deferred_tools.is_empty() {
            let specs: Vec<ToolSpec> = eager_tools.iter().map(|(s, _)| s.clone()).collect();
            return self.complete(messages, &specs).await;
        }
        self.run_search_loop(messages, eager_tools, deferred_tools, &[], 0)
            .await
    }

    async fn stream_with_search(
        &self,
        messages: &[Message],
        eager_tools: &[SpecWithHint],
        deferred_tools: &[SpecWithHint],
        stream_tx: Option<StreamSender>,
    ) -> Result<Completion> {
        if deferred_tools.is_empty() {
            let specs: Vec<ToolSpec> = eager_tools.iter().map(|(s, _)| s.clone()).collect();
            let tx = stream_tx.or_else(|| self.stream_tx.clone());
            return self.stream_inner(messages, &specs, tx).await;
        }
        self.run_stream_search_loop(messages, eager_tools, deferred_tools, &[], 0, stream_tx)
            .await
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
    fn tool_search_spec() -> SpecWithHint {
        let spec = ToolSpec {
            name: TOOL_SEARCH_TOOL_NAME.to_string(),
            description: "Fetches full schema definitions for deferred tools so they can be \
                called. Until fetched, only the name is known — there is no parameter schema, so \
                the tool cannot be invoked. Use `select:<tool_name>` for direct selection, or \
                keywords to search."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Query to find deferred tools."
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
        (spec, Some("find search discover tools lookup".to_string()))
    }

    /// Software-layer ToolSearch loop for `complete_with_search`.
    ///
    /// Sends only `eager_tools` + `ToolSearchTool` in round 0. If the model
    /// calls `ToolSearchTool`, resolves the query, appends the matched schemas
    /// as a plain JSON `tool_result`, and recurses with the matched tools
    /// added to `loaded_tools` (which are appended to eager for the next call).
    async fn run_search_loop(
        &self,
        messages: &[Message],
        eager_tools: &[SpecWithHint],
        deferred_tools: &[SpecWithHint],
        loaded_tools: &[ToolSpec],
        round: usize,
    ) -> Result<Completion> {
        // Build tool list: ToolSearchTool + caller eager + already-loaded deferred
        let mut call_tools: Vec<ToolSpec> = Vec::new();
        call_tools.push(Self::tool_search_spec().0);
        call_tools.extend(eager_tools.iter().map(|(s, _)| s.clone()));
        call_tools.extend_from_slice(loaded_tools);

        let completion = self.complete(messages, &call_tools).await?;

        let search_call = completion
            .tool_calls
            .iter()
            .find(|c| c.name == TOOL_SEARCH_TOOL_NAME);

        let search_call = match search_call {
            None => return Ok(completion),
            Some(c) => c.clone(),
        };

        if round >= self.max_search_rounds {
            tracing::warn!(
                target: "recursive::llm",
                round,
                max = self.max_search_rounds,
                "ToolSearchTool (openai): hit max_search_rounds, returning current completion"
            );
            return Ok(completion);
        }

        let query = search_call
            .arguments
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let names = self.search_engine.resolve(query, deferred_tools);
        let matched: Vec<ToolSpec> = deferred_tools
            .iter()
            .filter(|(s, _)| names.contains(&s.name))
            .map(|(s, _)| s.clone())
            .collect();
        let result_json = serde_json::to_string(&matched).unwrap_or_else(|_| "[]".into());

        // Append the ToolSearch exchange to the transcript
        let mut next_messages: Vec<Message> = messages.to_vec();
        next_messages.push(Message {
            role: Role::Assistant,
            content: completion.content.clone(),
            tool_calls: completion.tool_calls.clone(),
            tool_call_id: None,
            reasoning_content: completion.reasoning_content.clone(),
            is_compaction_summary: false,
        });
        next_messages.push(Message {
            role: Role::Tool,
            content: result_json,
            tool_calls: vec![],
            tool_call_id: Some(search_call.id.clone()),
            reasoning_content: None,
            is_compaction_summary: false,
        });

        let mut next_loaded = loaded_tools.to_vec();
        next_loaded.extend_from_slice(&matched);

        Box::pin(self.run_search_loop(
            &next_messages,
            eager_tools,
            deferred_tools,
            &next_loaded,
            round + 1,
        ))
        .await
    }

    /// Software-layer ToolSearch loop for `stream_with_search`.
    async fn run_stream_search_loop(
        &self,
        messages: &[Message],
        eager_tools: &[SpecWithHint],
        deferred_tools: &[SpecWithHint],
        loaded_tools: &[ToolSpec],
        round: usize,
        stream_tx: Option<StreamSender>,
    ) -> Result<Completion> {
        let mut call_tools: Vec<ToolSpec> = Vec::new();
        call_tools.push(Self::tool_search_spec().0);
        call_tools.extend(eager_tools.iter().map(|(s, _)| s.clone()));
        call_tools.extend_from_slice(loaded_tools);

        let tx = stream_tx.clone().or_else(|| self.stream_tx.clone());
        let completion = self.stream_inner(messages, &call_tools, tx).await?;

        let search_call = completion
            .tool_calls
            .iter()
            .find(|c| c.name == TOOL_SEARCH_TOOL_NAME);

        let search_call = match search_call {
            None => return Ok(completion),
            Some(c) => c.clone(),
        };

        if round >= self.max_search_rounds {
            tracing::warn!(
                target: "recursive::llm",
                round,
                max = self.max_search_rounds,
                "ToolSearchTool (openai/stream): hit max_search_rounds"
            );
            return Ok(completion);
        }

        let query = search_call
            .arguments
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let names = self.search_engine.resolve(query, deferred_tools);
        let matched: Vec<ToolSpec> = deferred_tools
            .iter()
            .filter(|(s, _)| names.contains(&s.name))
            .map(|(s, _)| s.clone())
            .collect();
        let result_json = serde_json::to_string(&matched).unwrap_or_else(|_| "[]".into());

        let mut next_messages: Vec<Message> = messages.to_vec();
        next_messages.push(Message {
            role: Role::Assistant,
            content: completion.content.clone(),
            tool_calls: completion.tool_calls.clone(),
            tool_call_id: None,
            reasoning_content: completion.reasoning_content.clone(),
            is_compaction_summary: false,
        });
        next_messages.push(Message {
            role: Role::Tool,
            content: result_json,
            tool_calls: vec![],
            tool_call_id: Some(search_call.id.clone()),
            reasoning_content: None,
            is_compaction_summary: false,
        });

        let mut next_loaded = loaded_tools.to_vec();
        next_loaded.extend_from_slice(&matched);

        Box::pin(self.run_stream_search_loop(
            &next_messages,
            eager_tools,
            deferred_tools,
            &next_loaded,
            round + 1,
            stream_tx,
        ))
        .await
    }

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

        // Stream requests need the raw Response object for SSE parsing, so we
        // can't use post_json_with_retry (which returns text). Retry only on
        // non-2xx and network errors; a successful 2xx hands off to parse_sse_stream.
        let mut attempt = 0;
        loop {
            tracing::debug!(target: "recursive::llm", request = %body, "POST {url} (stream)");
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
                    return Err(self.make_err(format!("HTTP {status}: {text}")));
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
        let mut reasoning_content = String::new();
        // key: index → (id, name, accumulated_arguments)
        let mut tool_call_builders: HashMap<usize, (String, String, String)> = HashMap::new();
        let mut finish_reason: Option<String> = None;
        let mut usage: Option<TokenUsage> = None;

        // Process the byte stream incrementally line by line.
        //
        // We maintain a raw byte buffer (`incomplete`) to handle multi-byte
        // UTF-8 sequences that may be split across HTTP chunk boundaries.
        // String::from_utf8_lossy() would silently replace the incomplete
        // tail bytes with U+FFFD, permanently corrupting non-ASCII content
        // (e.g. Chinese/Japanese text in tool arguments or assistant output).
        let mut byte_stream = resp.bytes_stream();
        let mut incomplete: Vec<u8> = Vec::new();
        let mut line_buf = String::new();

        while let Some(chunk) = byte_stream.next().await {
            let bytes = chunk.map_err(|e| self.make_err(format!("SSE stream read error: {e}")))?;
            // Prepend any leftover bytes from the previous chunk.
            let combined: Vec<u8> = if incomplete.is_empty() {
                bytes.to_vec()
            } else {
                let mut v = std::mem::take(&mut incomplete);
                v.extend_from_slice(&bytes);
                v
            };
            // Decode as much valid UTF-8 as possible; keep the remainder.
            let valid_up_to = match std::str::from_utf8(&combined) {
                Ok(_) => combined.len(),
                Err(e) => e.valid_up_to(),
            };
            incomplete = combined[valid_up_to..].to_vec();
            // SAFETY: valid_up_to is guaranteed by from_utf8 to be a valid
            // UTF-8 boundary within `combined`.
            let text = unsafe { std::str::from_utf8_unchecked(&combined[..valid_up_to]) };
            for ch in text.chars() {
                if ch == '\n' {
                    let line = std::mem::take(&mut line_buf);
                    Self::process_sse_line(
                        &line,
                        &mut content,
                        &mut reasoning_content,
                        &mut tool_call_builders,
                        &mut finish_reason,
                        &mut usage,
                        &stream_tx,
                    )?;
                } else if ch != '\r' {
                    line_buf.push(ch);
                }
            }
        }
        // Flush any remaining bytes (shouldn't happen with a well-formed stream,
        // but handle gracefully to avoid silent truncation).
        if !incomplete.is_empty() {
            tracing::warn!(
                bytes = incomplete.len(),
                "SSE stream ended with incomplete UTF-8 sequence; discarding tail bytes"
            );
        }
        // Flush any trailing line without a terminating newline.
        if !line_buf.is_empty() {
            Self::process_sse_line(
                &line_buf,
                &mut content,
                &mut reasoning_content,
                &mut tool_call_builders,
                &mut finish_reason,
                &mut usage,
                &stream_tx,
            )?;
        }

        // Convert accumulated builders into ToolCall objects, sorted by index.
        let mut sorted_indices: Vec<usize> = tool_call_builders.keys().copied().collect();
        sorted_indices.sort_unstable();
        let tool_calls: Vec<ToolCall> = sorted_indices
            .into_iter()
            .filter_map(|i| {
                let (id, name, partial_json) = tool_call_builders.remove(&i)?;
                if id.is_empty() && name.is_empty() {
                    return None;
                }
                let arguments = if partial_json.trim().is_empty() {
                    Value::Object(Default::default())
                } else {
                    serde_json::from_str(&partial_json).unwrap_or(Value::String(partial_json))
                };
                Some(ToolCall {
                    id,
                    name,
                    arguments,
                })
            })
            .collect();

        Ok(Completion {
            content,
            tool_calls,
            finish_reason,
            usage,
            reasoning_content: if reasoning_content.is_empty() {
                None
            } else {
                Some(reasoning_content)
            },
        })
    }

    fn process_sse_line(
        line: &str,
        content: &mut String,
        reasoning_content: &mut String,
        tool_call_builders: &mut HashMap<usize, (String, String, String)>,
        finish_reason: &mut Option<String>,
        usage: &mut Option<TokenUsage>,
        stream_tx: &Option<StreamSender>,
    ) -> Result<()> {
        let data = match line.strip_prefix("data: ") {
            Some(d) => d,
            None => return Ok(()),
        };
        if data.trim() == "[DONE]" {
            return Ok(());
        }

        let chunk: Value = serde_json::from_str(data).map_err(|e| Error::Llm {
            provider: "openai".into(),
            message: format!("SSE parse error: {e}; data: {data}"),
        })?;

        if let Some(choices) = chunk.get("choices").and_then(|c| c.as_array()) {
            if let Some(choice) = choices.first() {
                if let Some(delta) = choice.get("delta") {
                    if let Some(delta_content) = delta.get("content").and_then(|c| c.as_str()) {
                        if !delta_content.is_empty() {
                            content.push_str(delta_content);
                            if let Some(tx) = stream_tx {
                                let _ = tx.send(StreamChunk::Text(delta_content.to_string()));
                            }
                        }
                    }
                    if let Some(delta_reasoning) =
                        delta.get("reasoning_content").and_then(|c| c.as_str())
                    {
                        if !delta_reasoning.is_empty() {
                            reasoning_content.push_str(delta_reasoning);
                            if let Some(tx) = stream_tx {
                                let _ =
                                    tx.send(StreamChunk::Reasoning(delta_reasoning.to_string()));
                            }
                        }
                    }
                    // Accumulate tool_calls deltas
                    if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                        for tc in tcs {
                            let idx =
                                tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                            let entry = tool_call_builders
                                .entry(idx)
                                .or_insert_with(|| (String::new(), String::new(), String::new()));
                            if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                                if !id.is_empty() {
                                    entry.0 = id.to_string();
                                }
                            }
                            if let Some(func) = tc.get("function") {
                                if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                                    if !name.is_empty() {
                                        entry.1 = name.to_string();
                                    }
                                }
                                if let Some(args) = func.get("arguments").and_then(|v| v.as_str()) {
                                    entry.2.push_str(args);
                                }
                            }
                        }
                    }
                }
                if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                    if !fr.is_empty() {
                        *finish_reason = Some(fr.to_string());
                    }
                }
            }
        }

        if let Some(u) = chunk.get("usage") {
            let prompt = u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
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
            // Goal 273: o1 / o3 family reports reasoning tokens under
            // Goal 273: the streaming path does not extract
            // completion_tokens_details.reasoning_tokens here. The
            // non-streaming ResponseUsage default is 0; the future
            // enhancement is to add the field to ResponseUsage and
            // pluck it in `to_token_usage`.
            *usage = Some(TokenUsage {
                prompt_tokens: prompt,
                completion_tokens: completion,
                total_tokens: total,
                cache_hit_tokens: cache_hit,
                cache_miss_tokens: cache_miss,
                reasoning_tokens: 0,
            });
        }
        Ok(())
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
    // Echo reasoning_content back to the API (required by DeepSeek thinking mode).
    // Only valid on Assistant messages — other roles have no reasoning field.
    if matches!(m.role, Role::Assistant) {
        if let Some(ref reasoning) = m.reasoning_content {
            obj.insert("reasoning_content".into(), Value::String(reasoning.clone()));
        }
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
            reasoning_tokens: 0,
            prompt_tokens: self.prompt_tokens.unwrap_or(0),
            completion_tokens: self.completion_tokens.unwrap_or(0),
            total_tokens: self.total_tokens.unwrap_or(0),
            cache_hit_tokens: self.prompt_cache_hit_tokens.unwrap_or(0),
            cache_miss_tokens: self.prompt_cache_miss_tokens.unwrap_or(0),
            // Goal 273: o1 / o3 family reports reasoning tokens
            // under `completion_tokens_details.reasoning_tokens`,
            // but the streaming path already captures that into
            // TokenUsage directly. The non-streaming ResponseUsage
            // has no such field, so default 0.
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
    reasoning_content: Option<String>,
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
    let reasoning_content = choice.message.reasoning_content;
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
        reasoning_content,
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
                        "function":{"name":"Read","arguments":"{\"path\":\"src/lib.rs\"}"}
                    }]
                },
                "finish_reason":"tool_calls"
            }]
        }"#;
        let parsed: ChatResponse = serde_json::from_str(raw).unwrap();
        let c = parse_completion(parsed.choices.into_iter().next().unwrap(), parsed.usage);
        assert_eq!(c.tool_calls.len(), 1);
        assert_eq!(c.tool_calls[0].name, "Read");
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
                name: "Write".into(),
                arguments: serde_json::json!({"path":"a","contents":"b"}),
            }],
        );
        let v = serialize_message(&msg);
        assert_eq!(v["tool_calls"][0]["function"]["name"], "Write");
        let args = v["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap();
        let decoded: Value = serde_json::from_str(args).unwrap();
        assert_eq!(decoded["contents"], "b");
    }

    // ---- ToolSearch loop tests ----

    #[derive(Debug)]
    struct AlwaysReturnsEngine(Vec<String>);
    impl ToolSearchEngine for AlwaysReturnsEngine {
        fn resolve(&self, _query: &str, _candidates: &[SpecWithHint]) -> Vec<String> {
            self.0.clone()
        }
    }

    fn make_tool_spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: format!("{name} tool"),
            parameters: serde_json::json!({"type":"object","properties":{},"required":[]}),
        }
    }

    #[tokio::test]
    async fn openai_search_loop_skips_when_no_deferred() {
        // With no deferred tools, run_search_loop should make exactly one
        // request (no ToolSearch round-trip). We use a mock server that
        // returns a plain text response and assert it's called exactly once.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        std::thread::spawn(move || {
            use std::io::{Read, Write};
            loop {
                if let Ok((mut stream, _)) = listener.accept() {
                    call_count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let mut buf = [0u8; 8192];
                    let _ = stream.read(&mut buf);
                    let body = r#"{"choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#;
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .unwrap();
                    stream.flush().unwrap();
                }
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let provider = OpenAiProvider::new(format!("http://{addr}"), "sk-noop", "m").unwrap();
        let eager = vec![(make_tool_spec("Read"), None)];
        let result = provider
            .complete_with_search(&[Message::user("hi")], &eager, &[])
            .await
            .unwrap();
        assert_eq!(result.content, "done");
        assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn openai_search_loop_resolves_deferred_tool() {
        // Two mock responses:
        // Round 0: model calls ToolSearchTool with query "MyTool"
        // Round 1: model returns normal content
        // After round 1 the tool list should include MyTool's schema.
        use std::sync::Mutex;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let captured_tools: std::sync::Arc<Mutex<Vec<String>>> =
            std::sync::Arc::new(Mutex::new(vec![]));
        let captured_clone = captured_tools.clone();
        let call_idx = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let call_idx_clone = call_idx.clone();

        std::thread::spawn(move || {
            use std::io::{Read, Write};
            loop {
                if let Ok((mut stream, _)) = listener.accept() {
                    let idx = call_idx_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let mut buf = [0u8; 65536];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    // Extract JSON body and record tool names
                    if let Some(start) = req.find("\r\n\r\n") {
                        let body_str = req[start + 4..].trim();
                        if let Ok(body) = serde_json::from_str::<serde_json::Value>(body_str) {
                            if let Some(tools) = body["tools"].as_array() {
                                let names: Vec<String> = tools
                                    .iter()
                                    .filter_map(|t| {
                                        t["function"]["name"].as_str().map(String::from)
                                    })
                                    .collect();
                                *captured_clone.lock().unwrap() = names;
                            }
                        }
                    }
                    let resp = if idx == 0 {
                        // First call: return a ToolSearchTool call
                        r#"{"choices":[{"message":{"role":"assistant","content":"","tool_calls":[{"id":"ts1","type":"function","function":{"name":"ToolSearchTool","arguments":"{\"query\":\"MyTool\"}"}}]},"finish_reason":"tool_calls"}]}"#
                    } else {
                        // Second call: return normal content
                        r#"{"choices":[{"message":{"role":"assistant","content":"found it"},"finish_reason":"stop"}]}"#
                    };
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        resp.len(),
                        resp
                    )
                    .unwrap();
                    stream.flush().unwrap();
                }
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let my_tool = make_tool_spec("MyTool");
        let deferred = vec![(my_tool.clone(), None)];
        let engine = Arc::new(AlwaysReturnsEngine(vec!["MyTool".to_string()]));
        let provider = OpenAiProvider::new(format!("http://{addr}"), "sk-noop", "m")
            .unwrap()
            .with_search_engine(engine);

        let result = provider
            .complete_with_search(&[Message::user("hi")], &[], &deferred)
            .await
            .unwrap();
        assert_eq!(result.content, "found it");
        // The second request should include MyTool's schema
        let tools = captured_tools.lock().unwrap().clone();
        assert!(
            tools.contains(&"MyTool".to_string()),
            "second request should include MyTool, got: {tools:?}"
        );
        assert_eq!(call_idx.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn openai_search_loop_caps_at_max_rounds() {
        // Mock server always returns a ToolSearchTool call.
        // Assert we stop after max_search_rounds and return.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        std::thread::spawn(move || {
            use std::io::{Read, Write};
            loop {
                if let Ok((mut stream, _)) = listener.accept() {
                    call_count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let mut buf = [0u8; 65536];
                    let _ = stream.read(&mut buf);
                    // Always respond with a ToolSearchTool call
                    let resp = r#"{"choices":[{"message":{"role":"assistant","content":"","tool_calls":[{"id":"ts1","type":"function","function":{"name":"ToolSearchTool","arguments":"{\"query\":\"anything\"}"}}]},"finish_reason":"tool_calls"}]}"#;
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        resp.len(),
                        resp
                    )
                    .unwrap();
                    stream.flush().unwrap();
                }
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let deferred = vec![(make_tool_spec("SomeTool"), None)];
        let engine = Arc::new(AlwaysReturnsEngine(vec![])); // returns nothing
        let provider = OpenAiProvider::new(format!("http://{addr}"), "sk-noop", "m")
            .unwrap()
            .with_search_engine(engine);

        let result = provider
            .complete_with_search(&[Message::user("hi")], &[], &deferred)
            .await
            .unwrap();
        // Should have made max_search_rounds + 1 calls and stopped
        let calls = call_count.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(calls, 3 + 1, "expected {} calls, got {calls}", 3 + 1);
        // The returned completion should be the last ToolSearchTool response
        assert!(!result.tool_calls.is_empty());
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

        let provider =
            OpenAiProvider::new(format!("http://{addr}"), "sk-noop", "test-model").unwrap();
        let err = provider
            .complete(&[Message::user("hi")], &[])
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("test-model"),
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

        let provider =
            OpenAiProvider::new(format!("http://{addr}"), "sk-noop", "test-model-http").unwrap();
        let err = provider
            .complete(&[Message::user("hi")], &[])
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("test-model-http"),
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
            OpenAiProvider::new(format!("http://{addr}"), "sk-noop", "test-stream-model").unwrap();
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
            reasoning_content: None,
        }]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let completion = provider
            .stream(&[Message::user("hi")], &[], Some(tx))
            .await
            .unwrap();
        assert_eq!(completion.content, "fallback works");
        // Should have received the full content as a single delta
        let delta = rx.recv().await.unwrap();
        assert_eq!(delta, StreamChunk::Text("fallback works".to_string()));
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
        // Non-rate-limit 4xx errors should not be retried
        assert_eq!(policy.backoff_for(0, Some(400), false), None);
        assert_eq!(policy.backoff_for(0, Some(401), false), None);
        assert_eq!(policy.backoff_for(0, Some(404), false), None);
        // 429 (rate limit) IS retried with backoff
        assert!(policy.backoff_for(0, Some(429), false).is_some());
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
            OpenAiProvider::new(format!("http://{addr}"), "sk-noop", "test-structured-model")
                .unwrap();

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
            OpenAiProvider::new(format!("http://{addr}"), "sk-noop", "test-structured-model")
                .unwrap();

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
