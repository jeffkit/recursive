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
                           Sends a text message to the agent and waits for completion, \
                           polling the task status as needed."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Base URL of the A2A server (e.g. 'https://agent.example.com'). \
                                        The tool posts to {url}/message:send and polls {url}/tasks/{id}."
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
                                        The tool polls every 2 seconds during this window.",
                        "default": 60
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

        // Normalise base URL — strip trailing slash.
        let base = url.trim_end_matches('/');

        let client = Self::build_client().map_err(|e| Error::Tool {
            name: "a2a_call".into(),
            message: format!("failed to build HTTP client: {e}"),
        })?;

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
}
