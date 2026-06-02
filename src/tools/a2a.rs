//! `a2a_call` tool — invoke a remote A2A (Agent2Agent v1.0) agent.
//!
//! A2A is an open protocol (Linux Foundation / originally Google) that lets
//! agents built by different vendors delegate tasks to each other via HTTP+JSON.
//!
//! # Protocol overview
//!
//! 1. Client sends `POST {base_url}/message:send` with a `SendMessageRequest`.
//! 2. Server returns either a direct `Message` (synchronous) or a `Task` (async).
//! 3. If a `Task` is returned, the client polls `GET {base_url}/tasks/{id}` until
//!    the task reaches a terminal state (`COMPLETED`, `FAILED`, etc.).
//! 4. Artifacts from the completed task contain the final output.
//!
//! Reference: <https://a2a-protocol.org/v1.0.0/specification/>

use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::tools::{Tool, ToolSideEffect};

// ---------------------------------------------------------------------------
// A2A data model (minimal subset for MVP)
// ---------------------------------------------------------------------------

/// A text, file, or data part inside a Message or Artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Part {
    /// Plain text content. Other part types (file, data) are ignored for now.
    #[serde(default)]
    pub text: Option<String>,
}

/// An artifact produced by the remote agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    #[serde(default)]
    pub parts: Vec<Part>,
}

/// Task lifecycle state values from the A2A v1.0 spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskState {
    #[serde(rename = "TASK_STATE_SUBMITTED")]
    Submitted,
    #[serde(rename = "TASK_STATE_WORKING")]
    Working,
    #[serde(rename = "TASK_STATE_COMPLETED")]
    Completed,
    #[serde(rename = "TASK_STATE_FAILED")]
    Failed,
    #[serde(rename = "TASK_STATE_CANCELED")]
    Canceled,
    #[serde(rename = "TASK_STATE_REJECTED")]
    Rejected,
    /// Catch-all for unknown states (forward compatibility).
    #[serde(other)]
    Unknown,
}

impl TaskState {
    fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Canceled | Self::Rejected | Self::Unknown
        )
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::Submitted => "TASK_STATE_SUBMITTED",
            Self::Working => "TASK_STATE_WORKING",
            Self::Completed => "TASK_STATE_COMPLETED",
            Self::Failed => "TASK_STATE_FAILED",
            Self::Canceled => "TASK_STATE_CANCELED",
            Self::Rejected => "TASK_STATE_REJECTED",
            Self::Unknown => "TASK_STATE_UNKNOWN",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskStatus {
    pub state: TaskState,
}

/// A task returned by `POST /message:send` or `GET /tasks/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub status: TaskStatus,
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
}

/// A direct message response (no task tracking needed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageResponse {
    #[serde(default)]
    pub parts: Vec<Part>,
}

/// Top-level response from `POST /message:send`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SendMessageResponse {
    /// Agent returned a Task (async processing).
    HasTask { task: Task },
    /// Agent returned a direct Message.
    HasMessage { message: MessageResponse },
}

// ---------------------------------------------------------------------------
// Helper: extract text from a list of Parts
// ---------------------------------------------------------------------------

fn parts_to_text(parts: &[Part]) -> String {
    parts
        .iter()
        .filter_map(|p| p.text.as_deref())
        .collect::<Vec<_>>()
        .join("\n")
}

fn artifacts_to_text(artifacts: &[Artifact]) -> String {
    artifacts
        .iter()
        .flat_map(|a| a.parts.iter())
        .filter_map(|p| p.text.as_deref())
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// A2aCallTool
// ---------------------------------------------------------------------------

/// Tool that calls a remote A2A v1.0 agent and returns its text output.
pub struct A2aCallTool;

impl A2aCallTool {
    pub fn new() -> Self {
        Self
    }

    /// Build a `reqwest::Client` with sane timeouts.
    fn build_client() -> std::result::Result<reqwest::Client, reqwest::Error> {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
    }
}

impl Default for A2aCallTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for A2aCallTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "a2a_call".into(),
            description: "Invoke a remote A2A (Agent2Agent v1.0) agent and return its response. \
                           Supports both polling mode (default) and real-time SSE streaming mode. \
                           In polling mode the tool sends a message and polls the task until done. \
                           In streaming mode it connects to the server's SSE stream and accumulates \
                           artifact text as it arrives."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Base URL of the A2A server (e.g. 'https://agent.example.com'). \
                                        Polling: posts to {url}/message:send and polls {url}/tasks/{id}. \
                                        Streaming: posts to {url}/message:stream."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "The text message to send to the remote agent."
                    },
                    "authorization": {
                        "type": "string",
                        "description": "Optional Authorization header value (e.g. 'Bearer <token>'). \
                                        Omit for public agents."
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Maximum seconds to wait for task completion (default: 60, max: 300). \
                                        In polling mode: polls every 2 seconds. \
                                        In streaming mode: max time to keep the SSE connection open.",
                        "default": 60
                    },
                    "streaming": {
                        "type": "boolean",
                        "description": "If true, use SSE streaming mode (POST {url}/message:stream). \
                                        The server must support A2A streaming. Default: false.",
                        "default": false
                    },
                    "async_mode": {
                        "type": "boolean",
                        "description": "If true, submit the task and return immediately with the task ID \
                                        without waiting for completion. Use a2a_task_check later to \
                                        retrieve the result (composable with schedule_wakeup). Default: false.",
                        "default": false
                    }
                },
                "required": ["url", "prompt"]
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::External
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let url = arguments["url"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "a2a_call".into(),
                message: "missing required parameter: url".into(),
            })?;
        let prompt = arguments["prompt"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "a2a_call".into(),
                message: "missing required parameter: prompt".into(),
            })?;
        let authorization = arguments["authorization"].as_str();
        let timeout_secs = arguments["timeout_secs"]
            .as_u64()
            .unwrap_or(60)
            .clamp(1, 300);
        let use_streaming = arguments["streaming"].as_bool().unwrap_or(false);
        let use_async = arguments["async_mode"].as_bool().unwrap_or(false);

        // Normalise base URL — strip trailing slash.
        let base = url.trim_end_matches('/');

        let client = Self::build_client().map_err(|e| Error::Tool {
            name: "a2a_call".into(),
            message: format!("failed to build HTTP client: {e}"),
        })?;

        if use_streaming {
            return execute_streaming(base, prompt, authorization, timeout_secs, &client).await;
        }

        // ── Step 1: POST /message:send ────────────────────────────────────
        let message_id = uuid::Uuid::new_v4().to_string();
        let send_url = format!("{base}/message:send");

        let mut req = client
            .post(&send_url)
            .header(CONTENT_TYPE, "application/a2a+json")
            .json(&json!({
                "message": {
                    "role": "ROLE_USER",
                    "parts": [{"text": prompt}],
                    "messageId": message_id
                }
            }));

        if let Some(auth) = authorization {
            req = req.header(AUTHORIZATION, auth);
        }

        let resp = req.send().await.map_err(|e| Error::Tool {
            name: "a2a_call".into(),
            message: format!("network error calling {send_url}: {e}"),
        })?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Ok(format!("ERROR: HTTP {status} from {send_url}: {body}"));
        }

        let body_text = resp.text().await.map_err(|e| Error::Tool {
            name: "a2a_call".into(),
            message: format!("failed to read response body: {e}"),
        })?;

        let send_resp: SendMessageResponse =
            serde_json::from_str(&body_text).map_err(|e| Error::Tool {
                name: "a2a_call".into(),
                message: format!("invalid A2A response: {e}\nbody: {body_text}"),
            })?;

        // ── Step 2: handle synchronous message response ───────────────────
        match send_resp {
            SendMessageResponse::HasMessage { message } => {
                let text = parts_to_text(&message.parts);
                return Ok(if text.is_empty() {
                    "(empty response from agent)".to_string()
                } else {
                    text
                });
            }
            SendMessageResponse::HasTask { mut task } => {
                // ── Async mode: return task ID + background poll script ──────
                if use_async {
                    let task_id = &task.id;
                    let state = task.status.state.as_str();
                    // Build a shell one-liner the agent can pass to run_background.
                    // When the task reaches a terminal state the script exits, and
                    // run_event_loop detects the completed background job and starts
                    // a new turn with the output as its goal.
                    let poll_cmd = format!(
                        "end=$(($(date +%s)+{timeout_secs})); \
while [ $(date +%s) -lt $end ]; do \
  r=$(curl -sf '{base}/tasks/{task_id}' 2>/dev/null || echo '{{}}'); \
  s=$(echo \"$r\" | python3 -c \"import sys,json;print(json.load(sys.stdin).get('status',{{}}).get('state','UNKNOWN'))\" 2>/dev/null || echo UNKNOWN); \
  case \"$s\" in TASK_STATE_COMPLETED|TASK_STATE_FAILED|TASK_STATE_CANCELED|TASK_STATE_REJECTED) \
    echo \"A2A task {task_id} finished: $s\"; echo \"$r\"; exit 0;; \
  esac; sleep 2; done; \
echo \"A2A task {task_id} timed out after {timeout_secs}s\""
                    );
                    return Ok(format!(
                        "TASK_ID: {task_id}\nSTATE: {state}\n\n\
To poll in background and auto-trigger a new turn on completion, \
call run_background with:\n  command: {poll_cmd}\n  name: a2a-{task_id}\n\n\
When the background job completes, a new turn will start automatically with the result."
                    ));
                }

                // ── Step 3: poll until terminal state ─────────────────────
                let deadline = Instant::now() + Duration::from_secs(timeout_secs);
                let task_url = format!("{base}/tasks/{}", task.id);

                while !task.status.state.is_terminal() {
                    if Instant::now() >= deadline {
                        return Ok(format!(
                            "ERROR: task {} timed out after {}s (last state: {})",
                            task.id,
                            timeout_secs,
                            task.status.state.as_str()
                        ));
                    }

                    tokio::time::sleep(Duration::from_secs(2)).await;

                    let mut poll_req = client.get(&task_url);
                    if let Some(auth) = authorization {
                        poll_req = poll_req.header(AUTHORIZATION, auth);
                    }

                    let poll_resp = poll_req.send().await.map_err(|e| Error::Tool {
                        name: "a2a_call".into(),
                        message: format!("network error polling {task_url}: {e}"),
                    })?;

                    let poll_status = poll_resp.status();
                    if !poll_status.is_success() {
                        let err_body = poll_resp.text().await.unwrap_or_default();
                        return Ok(format!(
                            "ERROR: HTTP {poll_status} polling {task_url}: {err_body}"
                        ));
                    }

                    let poll_body = poll_resp.text().await.map_err(|e| Error::Tool {
                        name: "a2a_call".into(),
                        message: format!("failed to read poll response: {e}"),
                    })?;

                    let updated: Task =
                        serde_json::from_str(&poll_body).map_err(|e| Error::Tool {
                            name: "a2a_call".into(),
                            message: format!("invalid task poll response: {e}\nbody: {poll_body}"),
                        })?;

                    task = updated;
                }

                // ── Step 4: extract result ─────────────────────────────────
                match task.status.state {
                    TaskState::Completed => {
                        let text = artifacts_to_text(&task.artifacts);
                        Ok(if text.is_empty() {
                            "(task completed with no artifact text)".to_string()
                        } else {
                            text
                        })
                    }
                    other => Ok(format!(
                        "ERROR: task {} ended with state {}",
                        task.id,
                        other.as_str()
                    )),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// SSE streaming implementation
// ---------------------------------------------------------------------------

/// Parse one SSE event block (multiple `key: value` lines separated by `\n`)
/// and extract any artifact text and terminal task state from it.
fn parse_sse_event(event_block: &str) -> (Option<String>, Option<TaskState>) {
    let mut data_lines: Vec<&str> = Vec::new();
    for line in event_block.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.trim());
        }
    }
    let data = data_lines.join("");
    if data.is_empty() {
        return (None, None);
    }

    let Ok(v): std::result::Result<Value, _> = serde_json::from_str(&data) else {
        return (None, None);
    };

    // Extract artifact text — handle both A2A REST and JSON-RPC response shapes.
    let mut text: Option<String> = None;
    let mut state: Option<TaskState> = None;

    // Shape 1: {"type":"TaskArtifactUpdateEvent","task":{...}}
    // Shape 2: {"result":{"kind":"artifact-update","artifact":{...}}}
    if let Some(artifacts) = v
        .pointer("/task/artifacts")
        .or_else(|| v.pointer("/result/artifact/parts"))
        .and_then(|a| a.as_array())
    {
        let extracted: String = artifacts
            .iter()
            .flat_map(|a| a["parts"].as_array().into_iter().flatten())
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("");
        if !extracted.is_empty() {
            text = Some(extracted);
        }
    } else if let Some(part_text) = v
        .pointer("/result/artifact/parts/0/text")
        .and_then(|v| v.as_str())
    {
        text = Some(part_text.to_string());
    }

    // Extract state — handle both shapes.
    let raw_state = v
        .pointer("/task/status/state")
        .or_else(|| v.pointer("/result/status/state"))
        .and_then(|s| s.as_str());

    if let Some(s) = raw_state {
        if let Ok(ts) = serde_json::from_value::<TaskState>(Value::String(s.to_string())) {
            state = Some(ts);
        }
    }

    (text, state)
}

/// Execute `a2a_call` in SSE streaming mode.
///
/// POSTs to `{base}/message:stream` with `Accept: text/event-stream` and reads
/// the SSE stream until a terminal task state is received or the timeout expires.
async fn execute_streaming(
    base: &str,
    prompt: &str,
    authorization: Option<&str>,
    timeout_secs: u64,
    client: &reqwest::Client,
) -> Result<String> {
    let stream_url = format!("{base}/message:stream");
    let message_id = uuid::Uuid::new_v4().to_string();
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);

    let mut req = client
        .post(&stream_url)
        .header(CONTENT_TYPE, "application/a2a+json")
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .json(&json!({
            "message": {
                "role": "ROLE_USER",
                "parts": [{"text": prompt}],
                "messageId": message_id
            }
        }));

    if let Some(auth) = authorization {
        req = req.header(AUTHORIZATION, auth);
    }

    let resp = req.send().await.map_err(|e| Error::Tool {
        name: "a2a_call".into(),
        message: format!("network error calling {stream_url}: {e}"),
    })?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Ok(format!("ERROR: HTTP {status} from {stream_url}: {body}"));
    }

    // Read SSE events chunk by chunk.
    let mut buffer = String::new();
    let mut accumulated_text = String::new();
    let mut final_state: Option<TaskState> = None;
    let mut resp = resp;

    loop {
        if Instant::now() >= deadline {
            let suffix = if accumulated_text.is_empty() {
                "(no text received before timeout)".to_string()
            } else {
                accumulated_text.clone()
            };
            return Ok(format!(
                "ERROR: SSE stream timed out after {timeout_secs}s. Partial output:\n{suffix}"
            ));
        }

        let chunk = tokio::time::timeout(Duration::from_secs(5), resp.chunk())
            .await
            .map_err(|_| Error::Tool {
                name: "a2a_call".into(),
                message: "SSE chunk read timed out (5s idle)".into(),
            })?
            .map_err(|e| Error::Tool {
                name: "a2a_call".into(),
                message: format!("SSE read error: {e}"),
            })?;

        let Some(bytes) = chunk else {
            // Connection closed by server.
            break;
        };

        if let Ok(text_chunk) = std::str::from_utf8(&bytes) {
            buffer.push_str(text_chunk);
        }

        // Process complete SSE event blocks (separated by \n\n or \r\n\r\n).
        while let Some(pos) = buffer.find("\n\n").or_else(|| buffer.find("\r\n\r\n")) {
            let sep_len = if buffer[pos..].starts_with("\r\n\r\n") {
                4
            } else {
                2
            };
            let event_block = buffer[..pos].to_string();
            buffer.drain(..pos + sep_len);

            let (text, state) = parse_sse_event(&event_block);
            if let Some(t) = text {
                accumulated_text.push_str(&t);
            }
            if let Some(s) = state {
                if s.is_terminal() {
                    final_state = Some(s);
                    break;
                }
            }
        }

        if final_state.is_some() {
            break;
        }
    }

    match final_state {
        Some(TaskState::Completed) | None => {
            if accumulated_text.is_empty() {
                Ok("(task completed with no artifact text)".to_string())
            } else {
                Ok(accumulated_text)
            }
        }
        Some(other) => Ok(format!(
            "ERROR: task ended with state {} (partial output: {})",
            other.as_str(),
            accumulated_text
        )),
    }
}

// ---------------------------------------------------------------------------
// Agent Card data model
// ---------------------------------------------------------------------------

/// Capability flags from the A2A Agent Card.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentCapabilities {
    /// Whether the agent supports streaming (SSE) via `sendSubscribe`.
    #[serde(default)]
    pub streaming: bool,
    /// Whether the agent supports push-notification callbacks.
    #[serde(rename = "pushNotifications", default)]
    pub push_notifications: bool,
}

/// One skill entry in the Agent Card.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSkill {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
}

/// Authentication scheme described in the Agent Card.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentAuthentication {
    pub schemes: Vec<String>,
}

/// Subset of the A2A v1.0 Agent Card returned by `GET /.well-known/agent.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCard {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub capabilities: AgentCapabilities,
    #[serde(default)]
    pub skills: Vec<AgentSkill>,
    pub authentication: Option<AgentAuthentication>,
}

impl AgentCard {
    /// Return a concise human-readable summary of this Agent Card.
    pub fn summary(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        let name = if self.name.is_empty() {
            "<unnamed>"
        } else {
            &self.name
        };
        lines.push(format!("Agent: {name}"));
        if !self.description.is_empty() {
            lines.push(format!("Description: {}", self.description));
        }
        lines.push(format!(
            "Streaming: {}",
            if self.capabilities.streaming {
                "supported"
            } else {
                "not supported"
            }
        ));
        lines.push(format!(
            "Push notifications: {}",
            if self.capabilities.push_notifications {
                "supported"
            } else {
                "not supported"
            }
        ));
        if let Some(auth) = &self.authentication {
            lines.push(format!("Auth: {}", auth.schemes.join(", ")));
        } else {
            lines.push("Auth: none".into());
        }
        if !self.skills.is_empty() {
            lines.push("Skills:".into());
            for skill in &self.skills {
                let desc = if skill.description.is_empty() {
                    String::new()
                } else {
                    format!(": {}", skill.description)
                };
                lines.push(format!("  - {}{}", skill.name, desc));
            }
        }
        lines.join("\n")
    }
}

// ---------------------------------------------------------------------------
// A2aCardTool
// ---------------------------------------------------------------------------

/// Tool that fetches and summarises a remote A2A Agent Card
/// (`GET /.well-known/agent.json`).
pub struct A2aCardTool;

impl A2aCardTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for A2aCardTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for A2aCardTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "a2a_card".into(),
            description: "Fetch and display the Agent Card of a remote A2A v1.0 agent. \
                           The card describes the agent's capabilities (streaming, push \
                           notifications), authentication scheme, and available skills. \
                           Call this before a2a_call to discover what the agent supports."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Base URL of the A2A server (e.g. 'https://agent.example.com'). \
                                        The tool fetches {url}/.well-known/agent.json."
                    },
                    "authorization": {
                        "type": "string",
                        "description": "Optional Authorization header value (e.g. 'Bearer <token>')."
                    }
                },
                "required": ["url"]
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::External
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let url = arguments["url"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "a2a_card".into(),
                message: "missing required parameter: url".into(),
            })?;
        let authorization = arguments["authorization"].as_str();

        let base = url.trim_end_matches('/');
        let card_url = format!("{base}/.well-known/agent.json");

        let client = A2aCallTool::build_client().map_err(|e| Error::Tool {
            name: "a2a_card".into(),
            message: format!("failed to build HTTP client: {e}"),
        })?;

        let mut req = client.get(&card_url);
        if let Some(auth) = authorization {
            req = req.header(AUTHORIZATION, auth);
        }

        let resp = req.send().await.map_err(|e| Error::Tool {
            name: "a2a_card".into(),
            message: format!("network error fetching {card_url}: {e}"),
        })?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Ok(format!("ERROR: HTTP {status} from {card_url}: {body}"));
        }

        let body_text = resp.text().await.map_err(|e| Error::Tool {
            name: "a2a_card".into(),
            message: format!("failed to read response body: {e}"),
        })?;

        let card: AgentCard = serde_json::from_str(&body_text).map_err(|e| Error::Tool {
            name: "a2a_card".into(),
            message: format!("invalid Agent Card JSON: {e}\nbody: {body_text}"),
        })?;

        Ok(card.summary())
    }
}

// ---------------------------------------------------------------------------
// A2aTaskCheckTool
// ---------------------------------------------------------------------------

/// Tool that checks the current state and artifacts of an A2A task.
///
/// Designed to be used together with `a2a_call` in `async_mode: true`, allowing
/// the agent to submit a long-running task and check on it later, optionally
/// combined with `schedule_wakeup` for periodic polling.
pub struct A2aTaskCheckTool;

impl A2aTaskCheckTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for A2aTaskCheckTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for A2aTaskCheckTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "a2a_task_check".into(),
            description: "Check the current status and artifacts of a previously submitted A2A task. \
                           Useful for one-off manual inspection. The preferred pattern for automatic \
                           async polling is: use a2a_call with async_mode=true (which provides a \
                           ready-to-run shell command), pass that command to run_background, and let \
                           run_event_loop auto-trigger a new turn when the background job completes."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Base URL of the A2A server (same url used in a2a_call)."
                    },
                    "task_id": {
                        "type": "string",
                        "description": "Task ID returned by a2a_call in async_mode (the value after 'TASK_ID: ')."
                    },
                    "authorization": {
                        "type": "string",
                        "description": "Optional Authorization header value (e.g. 'Bearer <token>')."
                    }
                },
                "required": ["url", "task_id"]
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::External
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let url = arguments["url"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "a2a_task_check".into(),
                message: "missing required parameter: url".into(),
            })?;
        let task_id = arguments["task_id"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "a2a_task_check".into(),
                message: "missing required parameter: task_id".into(),
            })?;
        let authorization = arguments["authorization"].as_str();

        let base = url.trim_end_matches('/');
        let task_url = format!("{base}/tasks/{task_id}");

        let client = A2aCallTool::build_client().map_err(|e| Error::Tool {
            name: "a2a_task_check".into(),
            message: format!("failed to build HTTP client: {e}"),
        })?;

        let mut req = client.get(&task_url);
        if let Some(auth) = authorization {
            req = req.header(AUTHORIZATION, auth);
        }

        let resp = req.send().await.map_err(|e| Error::Tool {
            name: "a2a_task_check".into(),
            message: format!("network error fetching {task_url}: {e}"),
        })?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Ok(format!("ERROR: HTTP {status} from {task_url}: {body}"));
        }

        let body_text = resp.text().await.map_err(|e| Error::Tool {
            name: "a2a_task_check".into(),
            message: format!("failed to read response body: {e}"),
        })?;

        let task: Task = serde_json::from_str(&body_text).map_err(|e| Error::Tool {
            name: "a2a_task_check".into(),
            message: format!("invalid task JSON: {e}\nbody: {body_text}"),
        })?;

        let state_str = task.status.state.as_str();
        let artifact_text = artifacts_to_text(&task.artifacts);

        if task.status.state.is_terminal() {
            if artifact_text.is_empty() {
                Ok(format!("STATE: {state_str}\n(no artifact text)"))
            } else {
                Ok(format!("STATE: {state_str}\n{artifact_text}"))
            }
        } else {
            Ok(format!(
                "STATE: {state_str}\n(task not yet complete; check again later)"
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// Spawn a one-shot mock HTTP server on an ephemeral port.
    /// `response_body` is the raw HTTP response (including headers) the server sends.
    fn spawn_mock_server(response_body: &'static str) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 8192];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(response_body.as_bytes());
                let _ = stream.flush();
            }
        });
        addr
    }

    /// Spawn a mock server that returns different responses for each request.
    /// Requests are served in order; extras after the list result in 500.
    fn spawn_sequential_mock_server(responses: Vec<&'static str>) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let responses = Arc::new(Mutex::new(responses.into_iter()));
        std::thread::spawn(move || {
            for _ in 0..100 {
                if let Ok((mut stream, _)) = listener.accept() {
                    let mut buf = [0u8; 8192];
                    let _ = stream.read(&mut buf);
                    let response = responses
                        .lock()
                        .unwrap()
                        .next()
                        .unwrap_or("HTTP/1.1 500 Internal Server Error\r\nContent-Length: 5\r\nConnection: close\r\n\r\nerror");
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
            }
        });
        // Give the server a moment to start
        std::thread::sleep(std::time::Duration::from_millis(20));
        addr
    }

    fn make_json_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/a2a+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    }

    // ── Unit tests for data model helpers ─────────────────────────────────

    #[test]
    fn parts_to_text_concatenates_text_parts() {
        let parts = vec![
            Part {
                text: Some("Hello".into()),
            },
            Part { text: None },
            Part {
                text: Some(" World".into()),
            },
        ];
        assert_eq!(parts_to_text(&parts), "Hello\n World");
    }

    #[test]
    fn task_state_is_terminal() {
        assert!(TaskState::Completed.is_terminal());
        assert!(TaskState::Failed.is_terminal());
        assert!(TaskState::Canceled.is_terminal());
        assert!(TaskState::Rejected.is_terminal());
        assert!(!TaskState::Submitted.is_terminal());
        assert!(!TaskState::Working.is_terminal());
    }

    #[test]
    fn missing_prompt_returns_bad_tool_args_error() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let tool = A2aCallTool::new();
        let err = rt
            .block_on(tool.execute(json!({"url": "http://localhost"})))
            .unwrap_err();
        assert!(matches!(err, Error::BadToolArgs { .. }));
    }

    #[test]
    fn missing_url_returns_bad_tool_args_error() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let tool = A2aCallTool::new();
        let err = rt
            .block_on(tool.execute(json!({"prompt": "hi"})))
            .unwrap_err();
        assert!(matches!(err, Error::BadToolArgs { .. }));
    }

    #[tokio::test]
    async fn immediate_message_response_returns_text() {
        let body = r#"{"message":{"parts":[{"text":"The weather is sunny."}]}}"#;
        let raw_resp = Box::leak(make_json_response(body).into_boxed_str());
        let addr = spawn_mock_server(raw_resp);
        tokio::time::sleep(Duration::from_millis(30)).await;

        let tool = A2aCallTool::new();
        let result = tool
            .execute(json!({
                "url": format!("http://{addr}"),
                "prompt": "What is the weather?"
            }))
            .await
            .unwrap();

        assert_eq!(result, "The weather is sunny.");
    }

    #[tokio::test]
    async fn completed_task_response_returns_artifact_text() {
        let body = r#"{"task":{"id":"t1","status":{"state":"TASK_STATE_COMPLETED"},"artifacts":[{"parts":[{"text":"Task done!"}]}]}}"#;
        let raw_resp = Box::leak(make_json_response(body).into_boxed_str());
        let addr = spawn_mock_server(raw_resp);
        tokio::time::sleep(Duration::from_millis(30)).await;

        let tool = A2aCallTool::new();
        let result = tool
            .execute(json!({
                "url": format!("http://{addr}"),
                "prompt": "Do the task"
            }))
            .await
            .unwrap();

        assert_eq!(result, "Task done!");
    }

    #[tokio::test]
    async fn working_task_polled_to_completion() {
        // First response: WORKING task
        let working_resp = make_json_response(
            r#"{"task":{"id":"t2","status":{"state":"TASK_STATE_WORKING"},"artifacts":[]}}"#,
        );
        // Second response (GET /tasks/t2): COMPLETED task
        let completed_resp = make_json_response(
            r#"{"id":"t2","status":{"state":"TASK_STATE_COMPLETED"},"artifacts":[{"parts":[{"text":"Polling done"}]}]}"#,
        );
        let working_resp: &'static str = Box::leak(working_resp.into_boxed_str());
        let completed_resp: &'static str = Box::leak(completed_resp.into_boxed_str());

        let addr = spawn_sequential_mock_server(vec![working_resp, completed_resp]);

        let tool = A2aCallTool::new();
        let result = tool
            .execute(json!({
                "url": format!("http://{addr}"),
                "prompt": "Do the task",
                "timeout_secs": 30
            }))
            .await
            .unwrap();

        assert_eq!(result, "Polling done");
    }

    #[tokio::test]
    async fn failed_task_returns_error_string() {
        let body = r#"{"task":{"id":"t3","status":{"state":"TASK_STATE_FAILED"},"artifacts":[]}}"#;
        let raw_resp = Box::leak(make_json_response(body).into_boxed_str());
        let addr = spawn_mock_server(raw_resp);
        tokio::time::sleep(Duration::from_millis(30)).await;

        let tool = A2aCallTool::new();
        let result = tool
            .execute(json!({
                "url": format!("http://{addr}"),
                "prompt": "Do the task"
            }))
            .await
            .unwrap();

        assert!(
            result.starts_with("ERROR: task t3 ended with state TASK_STATE_FAILED"),
            "got: {result}"
        );
    }

    #[tokio::test]
    async fn http_error_returns_error_string() {
        let raw_resp = "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndown";
        let addr = spawn_mock_server(raw_resp);
        tokio::time::sleep(Duration::from_millis(30)).await;

        let tool = A2aCallTool::new();
        let result = tool
            .execute(json!({
                "url": format!("http://{addr}"),
                "prompt": "ping"
            }))
            .await
            .unwrap();

        assert!(result.starts_with("ERROR: HTTP"), "got: {result}");
    }

    // ── A2aCardTool tests ─────────────────────────────────────────────────

    #[test]
    fn agent_card_summary_full() {
        let card = AgentCard {
            name: "Weather Bot".into(),
            description: "Provides forecasts.".into(),
            capabilities: AgentCapabilities {
                streaming: true,
                push_notifications: false,
            },
            skills: vec![AgentSkill {
                id: "s1".into(),
                name: "get_weather".into(),
                description: "Get current weather.".into(),
            }],
            authentication: Some(AgentAuthentication {
                schemes: vec!["Bearer".into()],
            }),
        };
        let summary = card.summary();
        assert!(summary.contains("Agent: Weather Bot"), "{summary}");
        assert!(summary.contains("Streaming: supported"), "{summary}");
        assert!(
            summary.contains("Push notifications: not supported"),
            "{summary}"
        );
        assert!(summary.contains("Auth: Bearer"), "{summary}");
        assert!(summary.contains("get_weather"), "{summary}");
    }

    #[test]
    fn agent_card_summary_empty_name_uses_unnamed() {
        let card = AgentCard {
            name: String::new(),
            description: String::new(),
            capabilities: AgentCapabilities::default(),
            skills: vec![],
            authentication: None,
        };
        let summary = card.summary();
        assert!(summary.contains("Agent: <unnamed>"), "{summary}");
        assert!(summary.contains("Auth: none"), "{summary}");
    }

    #[tokio::test]
    async fn a2a_card_parses_valid_agent_card() {
        let body = r#"{
            "name": "Echo Agent",
            "description": "Echoes your message.",
            "capabilities": {"streaming": false, "pushNotifications": false},
            "skills": [{"id": "echo", "name": "echo", "description": "Echo text."}]
        }"#;
        let raw_resp = Box::leak(make_json_response(body).into_boxed_str());
        let addr = spawn_mock_server(raw_resp);
        tokio::time::sleep(Duration::from_millis(30)).await;

        let tool = A2aCardTool::new();
        let result = tool
            .execute(json!({"url": format!("http://{addr}")}))
            .await
            .unwrap();

        assert!(result.contains("Echo Agent"), "{result}");
        assert!(result.contains("Streaming: not supported"), "{result}");
        assert!(result.contains("echo"), "{result}");
    }

    #[tokio::test]
    async fn a2a_card_returns_error_on_http_404() {
        let raw_resp =
            "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot Found";
        let addr = spawn_mock_server(raw_resp);
        tokio::time::sleep(Duration::from_millis(30)).await;

        let tool = A2aCardTool::new();
        let result = tool
            .execute(json!({"url": format!("http://{addr}")}))
            .await
            .unwrap();

        assert!(result.starts_with("ERROR: HTTP 404"), "{result}");
    }

    // ── SSE streaming tests ────────────────────────────────────────────────

    #[test]
    fn parse_sse_event_extracts_artifact_text() {
        let event = r#"data: {"type":"TaskArtifactUpdateEvent","task":{"id":"t1","status":{"state":"TASK_STATE_WORKING"},"artifacts":[{"parts":[{"text":"Hello world"}],"index":0}]}}"#;
        let (text, state) = parse_sse_event(event);
        assert_eq!(text.as_deref(), Some("Hello world"));
        assert!(state.is_none() || matches!(state, Some(TaskState::Working)));
    }

    #[test]
    fn parse_sse_event_extracts_terminal_state() {
        let event = r#"data: {"type":"TaskStatusUpdateEvent","task":{"id":"t1","status":{"state":"TASK_STATE_COMPLETED"},"artifacts":[]}}"#;
        let (text, state) = parse_sse_event(event);
        assert!(text.is_none() || text.as_deref() == Some(""));
        assert!(matches!(state, Some(TaskState::Completed)));
    }

    #[test]
    fn parse_sse_event_ignores_unknown_data() {
        let event = "data: {\"not_a2a\": true}";
        let (text, state) = parse_sse_event(event);
        assert!(text.is_none());
        assert!(state.is_none());
    }

    #[tokio::test]
    async fn streaming_mode_accumulates_artifact_text() {
        // Build a minimal SSE response with two artifact events and a COMPLETED status.
        let body_parts = [
            "data: {\"type\":\"TaskStatusUpdateEvent\",\"task\":{\"id\":\"s1\",\"status\":{\"state\":\"TASK_STATE_WORKING\"},\"artifacts\":[]}}\n\n",
            "data: {\"type\":\"TaskArtifactUpdateEvent\",\"task\":{\"id\":\"s1\",\"status\":{\"state\":\"TASK_STATE_WORKING\"},\"artifacts\":[{\"parts\":[{\"text\":\"Hello \"}],\"index\":0}]}}\n\n",
            "data: {\"type\":\"TaskArtifactUpdateEvent\",\"task\":{\"id\":\"s1\",\"status\":{\"state\":\"TASK_STATE_WORKING\"},\"artifacts\":[{\"parts\":[{\"text\":\"world!\"}],\"index\":0}]}}\n\n",
            "data: {\"type\":\"TaskStatusUpdateEvent\",\"task\":{\"id\":\"s1\",\"status\":{\"state\":\"TASK_STATE_COMPLETED\"},\"artifacts\":[]}}\n\n",
        ].concat();
        let sse_body = body_parts.as_str();
        let raw_resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            sse_body.len(),
            sse_body
        );
        let raw_resp: &'static str = Box::leak(raw_resp.into_boxed_str());
        let addr = spawn_mock_server(raw_resp);
        tokio::time::sleep(Duration::from_millis(30)).await;

        let tool = A2aCallTool::new();
        let result = tool
            .execute(json!({
                "url": format!("http://{addr}"),
                "prompt": "say hello",
                "streaming": true,
                "timeout_secs": 10
            }))
            .await
            .unwrap();

        assert_eq!(result, "Hello world!", "got: {result}");
    }

    // ── Goal 179: async_mode + a2a_task_check tests ───────────────────────

    #[tokio::test]
    async fn async_mode_returns_task_id_immediately() {
        let body = r#"{"task":{"id":"async-t1","status":{"state":"TASK_STATE_SUBMITTED"},"artifacts":[]}}"#;
        let raw_resp = Box::leak(make_json_response(body).into_boxed_str());
        let addr = spawn_mock_server(raw_resp);
        tokio::time::sleep(Duration::from_millis(30)).await;

        let tool = A2aCallTool::new();
        let result = tool
            .execute(json!({
                "url": format!("http://{addr}"),
                "prompt": "do something slow",
                "async_mode": true
            }))
            .await
            .unwrap();

        assert!(result.contains("TASK_ID: async-t1"), "got: {result}");
        assert!(result.contains("TASK_STATE_SUBMITTED"), "got: {result}");
        assert!(result.contains("run_background"), "got: {result}");
        assert!(result.contains("curl"), "got: {result}");
    }

    #[tokio::test]
    async fn a2a_task_check_completed_task() {
        let body = r#"{"id":"check-t1","status":{"state":"TASK_STATE_COMPLETED"},"artifacts":[{"parts":[{"text":"Done!"}]}]}"#;
        let raw_resp = Box::leak(make_json_response(body).into_boxed_str());
        let addr = spawn_mock_server(raw_resp);
        tokio::time::sleep(Duration::from_millis(30)).await;

        let tool = A2aTaskCheckTool::new();
        let result = tool
            .execute(json!({
                "url": format!("http://{addr}"),
                "task_id": "check-t1"
            }))
            .await
            .unwrap();

        assert!(result.contains("TASK_STATE_COMPLETED"), "got: {result}");
        assert!(result.contains("Done!"), "got: {result}");
    }

    #[tokio::test]
    async fn a2a_task_check_working_task_hints_to_retry() {
        let body = r#"{"id":"check-t2","status":{"state":"TASK_STATE_WORKING"},"artifacts":[]}"#;
        let raw_resp = Box::leak(make_json_response(body).into_boxed_str());
        let addr = spawn_mock_server(raw_resp);
        tokio::time::sleep(Duration::from_millis(30)).await;

        let tool = A2aTaskCheckTool::new();
        let result = tool
            .execute(json!({
                "url": format!("http://{addr}"),
                "task_id": "check-t2"
            }))
            .await
            .unwrap();

        assert!(result.contains("TASK_STATE_WORKING"), "got: {result}");
        assert!(result.contains("check again later"), "got: {result}");
    }

    #[tokio::test]
    async fn a2a_task_check_http_error_returns_error_string() {
        let raw_resp =
            "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot Found";
        let addr = spawn_mock_server(raw_resp);
        tokio::time::sleep(Duration::from_millis(30)).await;

        let tool = A2aTaskCheckTool::new();
        let result = tool
            .execute(json!({
                "url": format!("http://{addr}"),
                "task_id": "missing-task"
            }))
            .await
            .unwrap();

        assert!(result.starts_with("ERROR: HTTP 404"), "got: {result}");
    }
}
