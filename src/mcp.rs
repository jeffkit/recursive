//! MCP (Model Context Protocol) client — stdio and HTTP+SSE transport, JSON-RPC 2.0.
//!
//! Supports the bounded subset needed for tool proxy:
//! - `initialize` / `initialized` handshake
//! - `tools/list` to discover tools
//! - `tools/call` to invoke them
//!
//! Also supports:
//! - `resources/list` and `resources/read`
//! - `prompts/list` and `prompts/get`

use async_trait::async_trait;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::tools::Tool;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Configuration for a single MCP server in the Claude Code `.mcp.json` format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Command for stdio transport (mutually exclusive with `url`).
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    /// URL for HTTP+SSE transport (mutually exclusive with `command`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Configuration for a single MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServer {
    pub name: String,
    /// Command for stdio transport, or empty if using HTTP+SSE.
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// URL for HTTP+SSE transport, or None if using stdio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Extra environment variables to set when spawning a stdio subprocess.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
}

impl From<(String, McpServerConfig)> for McpServer {
    fn from((name, config): (String, McpServerConfig)) -> Self {
        Self {
            name,
            command: config.command,
            args: config.args,
            url: config.url,
            env: config.env,
        }
    }
}

/// Optional 2025-03-26 MCP tool annotations that carry hints about
/// the tool's side-effects. All fields default to `false` / `None`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolAnnotations {
    /// Hints the tool only reads data without side-effects.
    #[serde(default)]
    pub read_only_hint: bool,
    /// Hints the tool may produce destructive side-effects.
    #[serde(default)]
    pub destructive_hint: bool,
    /// Hints the tool is idempotent.
    #[serde(default)]
    pub idempotent_hint: bool,
    /// Hints the tool interacts with the open world (network, I/O).
    #[serde(default)]
    pub open_world_hint: bool,
}

/// Per-MCP-server trust level. Controls whether annotation hints from
/// a server are believed when classifying side-effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerTrust {
    /// Annotations are trusted and used to derive [`ToolSideEffect`].
    Trusted,
    /// Annotations are ignored; all tools default to `External`.
    /// This is the conservative default.
    #[default]
    Untrusted,
}

/// A tool spec as returned by the MCP server's `tools/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub input_schema: Value,
    /// Optional 2025-03-26 annotations field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<McpToolAnnotations>,
}

/// A resource exposed by an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpResource {
    pub uri: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "mimeType")]
    pub mime_type: Option<String>,
}

/// Content returned from reading an MCP resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpResourceContent {
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "mimeType")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

/// A prompt template exposed by an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpPrompt {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Vec<McpPromptArgument>>,
}

/// An argument to an MCP prompt template.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpPromptArgument {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
}

/// A message in an MCP prompt response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpPromptMessage {
    pub role: String,
    pub content: String,
}

// ---------------------------------------------------------------------------
// Transport abstraction
// ---------------------------------------------------------------------------

/// The underlying transport for an MCP client.
#[allow(clippy::large_enum_variant)]
enum McpTransport {
    /// Stdio subprocess transport.
    Stdio {
        stdin: ChildStdin,
        reader: BufReader<ChildStdout>,
        child: Option<Child>,
    },
    /// HTTP+SSE transport.
    HttpSse {
        client: reqwest::Client,
        /// Base SSE endpoint URL (the one that returns the event stream).
        sse_url: String,
        /// URL template for POST requests (from the `endpoint` event).
        post_url: Option<String>,
        /// Buffer for accumulating SSE data between reads.
        buffer: String,
    },
}

impl fmt::Debug for McpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdio { .. } => f.debug_struct("Stdio").finish(),
            Self::HttpSse {
                sse_url, post_url, ..
            } => f
                .debug_struct("HttpSse")
                .field("sse_url", sse_url)
                .field("post_url", post_url)
                .finish(),
        }
    }
}

/// MCP elicitation request forwarded to a host (Claude `elicitation` control).
#[derive(Debug, Clone)]
pub struct ElicitationRequest {
    pub mcp_server_name: String,
    pub message: String,
    pub mode: Option<String>,
    pub url: Option<String>,
    pub elicitation_id: Option<String>,
    pub requested_schema: Option<Value>,
    pub title: Option<String>,
    pub display_name: Option<String>,
    pub description: Option<String>,
}

/// Handles MCP `-32042` UrlElicitationRequired by asking the host.
#[async_trait]
pub trait ElicitationHandler: Send + Sync {
    async fn elicit(&self, request: ElicitationRequest) -> Option<Value>;
}

/// Shared slot so the CLI control channel can install a handler after MCP
/// clients are constructed.
pub type SharedElicitationHandler = Arc<tokio::sync::RwLock<Option<Arc<dyn ElicitationHandler>>>>;

/// Create an empty elicitation-handler slot.
pub fn new_elicitation_slot() -> SharedElicitationHandler {
    Arc::new(tokio::sync::RwLock::new(None))
}

/// An MCP client owns a transport and manages JSON-RPC communication.
pub struct McpClient {
    transport: McpTransport,
    next_id: u64,
    /// Capabilities advertised by the server during initialization.
    capabilities: ServerCapabilities,
    /// Name of the MCP server (for error reporting).
    server_name: String,
    /// How long to wait for a single line of stdio output before
    /// declaring the server hung. Defaults to
    /// [`DEFAULT_STDIO_READ_TIMEOUT`]; tests use a shorter value to
    /// keep the suite fast.
    read_timeout: Duration,
    /// Optional host elicitation handler (Claude control channel).
    elicitation: Option<SharedElicitationHandler>,
}

/// Default timeout for reading a single JSON-RPC line from a stdio
/// MCP server. Generous on purpose — real servers can pause briefly
/// during large tool runs without being broken.
pub const DEFAULT_STDIO_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Capabilities advertised by an MCP server during the initialize handshake.
#[derive(Debug, Clone, Default)]
pub struct ServerCapabilities {
    pub tools: bool,
    pub resources: bool,
    pub prompts: bool,
}

impl fmt::Debug for McpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpClient")
            .field("transport", &self.transport)
            .field("next_id", &self.next_id)
            .field("capabilities", &self.capabilities)
            .finish()
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        if let McpTransport::Stdio { ref mut child, .. } = &mut self.transport {
            if let Some(mut child) = child.take() {
                let _ = child.start_kill();
            }
        }
    }
}

impl McpClient {
    /// Spawn an MCP server (stdio or HTTP+SSE), perform the initialize
    /// handshake, and return a ready-to-use client.
    ///
    /// If `server.url` is set, uses HTTP+SSE transport. Otherwise uses stdio.
    pub async fn spawn(server: &McpServer) -> Result<Self> {
        Self::spawn_with_timeout(server, DEFAULT_STDIO_READ_TIMEOUT).await
    }

    /// Like [`spawn`], but with a custom stdio read timeout. Tests use
    /// a short value (e.g. 500ms) to keep the suite fast; production
    /// callers should stick to [`spawn`].
    pub async fn spawn_with_timeout(server: &McpServer, read_timeout: Duration) -> Result<Self> {
        if let Some(url) = &server.url {
            // HTTP+SSE has its own internal timeouts that are independent
            // of stdio handshake behavior; keep its existing semantics.
            Self::spawn_http_sse(server, url).await
        } else {
            Self::spawn_stdio(server, read_timeout).await
        }
    }

    /// Spawn via stdio subprocess.
    async fn spawn_stdio(server: &McpServer, read_timeout: Duration) -> Result<Self> {
        let mut cmd = Command::new(&server.command);
        cmd.args(&server.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        if let Some(env) = &server.env {
            cmd.envs(env);
        }
        let mut child = cmd.spawn().map_err(|e| Error::Mcp {
            server: server.name.clone(),
            message: format!("failed to spawn: {e}"),
        })?;

        let stdin = child.stdin.take().ok_or_else(|| Error::Mcp {
            server: server.name.clone(),
            message: "failed to open stdin".into(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| Error::Mcp {
            server: server.name.clone(),
            message: "failed to open stdout".into(),
        })?;
        let reader = BufReader::new(stdout);

        let mut client = Self {
            transport: McpTransport::Stdio {
                stdin,
                reader,
                child: Some(child),
            },
            next_id: 1,
            capabilities: ServerCapabilities::default(),
            server_name: server.name.clone(),
            read_timeout,
            elicitation: None,
        };

        client.do_initialize(&server.name).await?;
        Ok(client)
    }

    /// Spawn via HTTP+SSE transport.
    async fn spawn_http_sse(server: &McpServer, url: &str) -> Result<Self> {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| Error::Mcp {
                server: server.name.clone(),
                message: format!("failed to build HTTP client: {e}"),
            })?;

        // Connect to the SSE endpoint and read the initial event to discover
        // the message endpoint.
        let response = http_client
            .get(url)
            .header("Accept", "text/event-stream")
            .send()
            .await
            .map_err(|e| Error::Mcp {
                server: server.name.clone(),
                message: format!("failed to connect to SSE endpoint `{url}`: {e}"),
            })?;

        if !response.status().is_success() {
            return Err(Error::Mcp {
                server: server.name.clone(),
                message: format!("SSE endpoint `{url}` returned HTTP {}", response.status()),
            });
        }

        // Read the SSE stream to find the `endpoint` event (which tells us
        // where to POST JSON-RPC messages). We buffer the stream for later use.
        let mut stream = response.bytes_stream();
        let mut sse_buffer = String::new();
        let mut post_url: Option<String> = None;
        let mut found_endpoint = false;

        // Read up to 64KB to find the endpoint event
        let mut total_read = 0usize;
        let max_read = 65536;

        while total_read < max_read {
            match timeout(Duration::from_secs(10), stream.next()).await {
                Ok(Some(Ok(chunk))) => {
                    let chunk_str = String::from_utf8_lossy(&chunk);
                    total_read += chunk_str.len();
                    sse_buffer.push_str(&chunk_str);

                    // Parse SSE events from the buffer
                    if let Some(endpoint) = parse_sse_endpoint(&sse_buffer) {
                        post_url = Some(endpoint);
                        found_endpoint = true;
                        break;
                    }
                }
                Ok(Some(Err(e))) => {
                    return Err(Error::Mcp {
                        server: server.name.clone(),
                        message: format!("error reading SSE stream from `{url}`: {e}"),
                    });
                }
                Ok(None) => break, // Stream ended
                Err(_) => {
                    return Err(Error::Mcp {
                        server: server.name.clone(),
                        message: format!("timeout reading SSE stream from `{url}`"),
                    });
                }
            }
        }

        if !found_endpoint {
            return Err(Error::Mcp {
                server: server.name.clone(),
                message: format!(
                    "SSE endpoint `{url}` did not send an `endpoint` event. Received data: {}",
                    &sse_buffer[..sse_buffer.len().min(200)]
                ),
            });
        }

        let mut client = Self {
            transport: McpTransport::HttpSse {
                client: http_client,
                sse_url: url.to_string(),
                post_url,
                buffer: sse_buffer,
            },
            next_id: 1,
            capabilities: ServerCapabilities::default(),
            server_name: server.name.clone(),
            // SSE transport reads its own stream with internal timeouts;
            // this field is unused for HTTP+SSE but must be set.
            read_timeout: DEFAULT_STDIO_READ_TIMEOUT,
            elicitation: None,
        };

        client.do_initialize(&server.name).await?;
        Ok(client)
    }

    /// Perform the MCP initialize handshake (common to both transports).
    async fn do_initialize(&mut self, server_name: &str) -> Result<()> {
        let init_result: Value = self
            .send_request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "recursive-agent",
                        "version": "0.1.0"
                    }
                }),
            )
            .await?;

        // Check protocol version in response
        if let Some(server_proto) = init_result.get("protocolVersion").and_then(|v| v.as_str()) {
            if server_proto != "2024-11-05" {
                // Non-fatal: log but continue
                tracing::warn!(
                    target: "recursive::mcp",
                    server = %server_name,
                    server_protocol = %server_proto,
                    "MCP server protocol version mismatch"
                );
            }
        }

        // Parse capabilities from the server response
        if let Some(caps) = init_result.get("capabilities") {
            self.capabilities.tools = caps.get("tools").and_then(|v| v.as_bool()).unwrap_or(false);
            self.capabilities.resources = caps
                .get("resources")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            self.capabilities.prompts = caps
                .get("prompts")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
        }

        // Send initialized notification (no response expected)
        self.send_notification("notifications/initialized", serde_json::json!({}))
            .await?;

        Ok(())
    }

    /// Call `tools/list` and return the discovered tool specs.
    pub async fn list_tools(&mut self) -> Result<Vec<McpToolSpec>> {
        let result: Value = self
            .send_request("tools/list", serde_json::json!({}))
            .await?;

        let tools_arr = result
            .get("tools")
            .and_then(|v| v.as_array())
            .ok_or_else(|| Error::Mcp {
                server: self.server_name.clone(),
                message: "`tools/list` response missing `tools` array".into(),
            })?;

        let mut specs = Vec::with_capacity(tools_arr.len());
        for item in tools_arr {
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| Error::Mcp {
                    server: self.server_name.clone(),
                    message: "tool entry missing `name`".into(),
                })?;
            let description = item
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let input_schema = item
                .get("inputSchema")
                .cloned()
                .unwrap_or(serde_json::json!({"type": "object"}));
            // Try to deserialize optional annotations field.
            let annotations = item
                .get("annotations")
                .and_then(|v| serde_json::from_value::<McpToolAnnotations>(v.clone()).ok());
            specs.push(McpToolSpec {
                name: name.to_string(),
                description: description.to_string(),
                input_schema,
                annotations,
            });
        }

        Ok(specs)
    }

    /// Attach a shared elicitation-handler slot (Claude control channel).
    pub fn with_elicitation(mut self, slot: SharedElicitationHandler) -> Self {
        self.elicitation = Some(slot);
        self
    }

    /// Call a tool by name with the given arguments.
    /// Returns the textual content of the first `content[].text` block.
    /// Errors if `isError` is true in the response.
    ///
    /// When the server returns JSON-RPC `-32042` (UrlElicitationRequired) and
    /// an [`ElicitationHandler`] is installed, the host is asked via the
    /// Claude control channel before the error is returned to the caller.
    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<String> {
        let result: Value = match self
            .send_request(
                "tools/call",
                serde_json::json!({
                    "name": name,
                    "arguments": arguments,
                }),
            )
            .await
        {
            Ok(v) => v,
            Err(Error::Mcp { server, message }) if message.contains("code -32042") => {
                // UrlElicitationRequired — ask the host, then surface a clear error
                // (retry is host-driven via mcp_call / re-invoke).
                if let Some(slot) = &self.elicitation {
                    let handler = slot.read().await.clone();
                    if let Some(h) = handler {
                        let req = parse_elicitation_from_error(&server, &message);
                        let _ = h.elicit(req).await;
                    }
                }
                return Err(Error::Mcp { server, message });
            }
            Err(e) => return Err(e),
        };

        // Check for error
        if result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            let error_msg = result
                .get("content")
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.first())
                .and_then(|c| c.get("text"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(Error::Tool {
                name: name.to_string(),
                call_id: None,
                message: error_msg.to_string(),
            });
        }

        // Extract text content
        let content = result
            .get("content")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|c| {
                        if c.get("type").and_then(|v| v.as_str()) == Some("text") {
                            c.get("text").and_then(|v| v.as_str())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();

        Ok(content)
    }

    /// Call `resources/list` and return the discovered resources.
    pub async fn list_resources(&mut self) -> Result<Vec<McpResource>> {
        if !self.capabilities.resources {
            return Err(Error::Mcp {
                server: self.server_name.clone(),
                message: "server does not advertise `resources` capability".into(),
            });
        }

        let result: Value = self
            .send_request("resources/list", serde_json::json!({}))
            .await?;

        let resources_arr = result
            .get("resources")
            .and_then(|v| v.as_array())
            .ok_or_else(|| Error::Mcp {
                server: self.server_name.clone(),
                message: "`resources/list` response missing `resources` array".into(),
            })?;

        let mut resources = Vec::with_capacity(resources_arr.len());
        for item in resources_arr {
            let resource: McpResource =
                serde_json::from_value(item.clone()).map_err(|e| Error::Mcp {
                    server: self.server_name.clone(),
                    message: format!("failed to parse resource: {e}"),
                })?;
            resources.push(resource);
        }

        Ok(resources)
    }

    /// Call `resources/read` for a specific resource URI.
    /// Returns the list of content items.
    pub async fn read_resource(&mut self, uri: &str) -> Result<Vec<McpResourceContent>> {
        if !self.capabilities.resources {
            return Err(Error::Mcp {
                server: self.server_name.clone(),
                message: "server does not advertise `resources` capability".into(),
            });
        }

        let result: Value = self
            .send_request("resources/read", serde_json::json!({ "uri": uri }))
            .await?;

        let contents_arr = result
            .get("contents")
            .and_then(|v| v.as_array())
            .ok_or_else(|| Error::Mcp {
                server: self.server_name.clone(),
                message: "`resources/read` response missing `contents` array".into(),
            })?;

        let mut contents = Vec::with_capacity(contents_arr.len());
        for item in contents_arr {
            let content: McpResourceContent =
                serde_json::from_value(item.clone()).map_err(|e| Error::Mcp {
                    server: self.server_name.clone(),
                    message: format!("failed to parse resource content: {e}"),
                })?;
            contents.push(content);
        }

        Ok(contents)
    }

    /// Call `prompts/list` and return the discovered prompts.
    pub async fn list_prompts(&mut self) -> Result<Vec<McpPrompt>> {
        if !self.capabilities.prompts {
            return Err(Error::Mcp {
                server: self.server_name.clone(),
                message: "server does not advertise `prompts` capability".into(),
            });
        }

        let result: Value = self
            .send_request("prompts/list", serde_json::json!({}))
            .await?;

        let prompts_arr = result
            .get("prompts")
            .and_then(|v| v.as_array())
            .ok_or_else(|| Error::Mcp {
                server: self.server_name.clone(),
                message: "`prompts/list` response missing `prompts` array".into(),
            })?;

        let mut prompts = Vec::with_capacity(prompts_arr.len());
        for item in prompts_arr {
            let prompt: McpPrompt =
                serde_json::from_value(item.clone()).map_err(|e| Error::Mcp {
                    server: self.server_name.clone(),
                    message: format!("failed to parse prompt: {e}"),
                })?;
            prompts.push(prompt);
        }

        Ok(prompts)
    }

    /// Call `prompts/get` for a specific prompt name with optional arguments.
    /// Returns the list of messages.
    pub async fn get_prompt(
        &mut self,
        name: &str,
        arguments: Option<HashMap<String, String>>,
    ) -> Result<Vec<McpPromptMessage>> {
        if !self.capabilities.prompts {
            return Err(Error::Mcp {
                server: self.server_name.clone(),
                message: "server does not advertise `prompts` capability".into(),
            });
        }

        let mut params = serde_json::json!({ "name": name });
        if let Some(args) = arguments {
            params["arguments"] = serde_json::to_value(args).map_err(|e| Error::Mcp {
                server: self.server_name.clone(),
                message: format!("failed to serialize prompt arguments: {e}"),
            })?;
        }

        let result: Value = self.send_request("prompts/get", params).await?;

        let messages_arr = result
            .get("messages")
            .and_then(|v| v.as_array())
            .ok_or_else(|| Error::Mcp {
                server: self.server_name.clone(),
                message: "`prompts/get` response missing `messages` array".into(),
            })?;

        let mut messages = Vec::with_capacity(messages_arr.len());
        for item in messages_arr {
            let role = item
                .get("role")
                .and_then(|v| v.as_str())
                .ok_or_else(|| Error::Mcp {
                    server: self.server_name.clone(),
                    message: "prompt message missing `role`".into(),
                })?;
            let content = item
                .get("content")
                .and_then(|v| v.get("text"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| Error::Mcp {
                    server: self.server_name.clone(),
                    message: "prompt message missing `content.text`".into(),
                })?;
            messages.push(McpPromptMessage {
                role: role.to_string(),
                content: content.to_string(),
            });
        }

        Ok(messages)
    }

    // -----------------------------------------------------------------------
    // JSON-RPC 2.0 internals
    // -----------------------------------------------------------------------

    /// Send a JSON-RPC request and await the matching response.
    async fn send_request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        self.write_line(&request).await?;
        self.read_response(id).await
    }

    /// Send a JSON-RPC notification (no response expected).
    async fn send_notification(&mut self, method: &str, params: Value) -> Result<()> {
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });

        self.write_line(&notification).await
    }

    /// Write a JSON-RPC message via the active transport.
    async fn write_line(&mut self, value: &Value) -> Result<()> {
        match &mut self.transport {
            McpTransport::Stdio { stdin, .. } => {
                let line = serde_json::to_string(value)?;
                let mut full = line.into_bytes();
                full.push(b'\n');
                stdin.write_all(&full).await.map_err(|e| Error::Mcp {
                    server: self.server_name.clone(),
                    message: format!("write error: {e}"),
                })?;
                stdin.flush().await.map_err(|e| Error::Mcp {
                    server: self.server_name.clone(),
                    message: format!("flush error: {e}"),
                })?;
                Ok(())
            }
            McpTransport::HttpSse {
                client, post_url, ..
            } => {
                let url = post_url.as_ref().ok_or_else(|| Error::Mcp {
                    server: self.server_name.clone(),
                    message: "HTTP transport: no POST endpoint available".into(),
                })?;
                let body = serde_json::to_string(value)?;
                let response = client
                    .post(url)
                    .header("Content-Type", "application/json")
                    .body(body)
                    .send()
                    .await
                    .map_err(|e| Error::Mcp {
                        server: self.server_name.clone(),
                        message: format!("HTTP POST error: {e}"),
                    })?;
                if !response.status().is_success() {
                    return Err(Error::Mcp {
                        server: self.server_name.clone(),
                        message: format!(
                            "HTTP POST to `{url}` returned HTTP {}",
                            response.status()
                        ),
                    });
                }
                Ok(())
            }
        }
    }

    /// Read a JSON-RPC response matching the given id from the active transport.
    async fn read_response(&mut self, expected_id: u64) -> Result<Value> {
        match &mut self.transport {
            McpTransport::Stdio { reader, .. } => {
                Self::read_stdio_response(reader, expected_id, &self.server_name, self.read_timeout)
                    .await
            }
            McpTransport::HttpSse {
                client,
                sse_url,
                post_url,
                buffer,
            } => {
                Self::read_sse_response(
                    client,
                    sse_url,
                    post_url,
                    buffer,
                    expected_id,
                    &self.server_name,
                )
                .await
            }
        }
    }

    /// Read a JSON-RPC response from stdio.
    async fn read_stdio_response(
        reader: &mut BufReader<ChildStdout>,
        expected_id: u64,
        server_name: &str,
        read_timeout: Duration,
    ) -> Result<Value> {
        let mut line_buf = String::new();

        loop {
            line_buf.clear();

            let read_future = reader.read_line(&mut line_buf);
            match timeout(read_timeout, read_future).await {
                Ok(Ok(0)) => {
                    return Err(Error::Mcp {
                        server: server_name.to_string(),
                        message: "server closed stdout unexpectedly".into(),
                    });
                }
                Ok(Ok(_)) => {
                    let trimmed = line_buf.trim();
                    if trimmed.is_empty() {
                        continue;
                    }

                    let parsed: Value = serde_json::from_str(trimmed).map_err(|e| Error::Mcp {
                        server: server_name.to_string(),
                        message: format!("server returned non-JSON line: {e}; line: {trimmed}"),
                    })?;

                    if let Some(resp_id) = parsed.get("id") {
                        if resp_id.as_u64() == Some(expected_id) {
                            if let Some(err) = parsed.get("error") {
                                let msg = err
                                    .get("message")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown error");
                                let code = err.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
                                return Err(Error::Mcp {
                                    server: server_name.to_string(),
                                    message: format!("error (code {code}): {msg}"),
                                });
                            }
                            if let Some(result) = parsed.get("result") {
                                return Ok(result.clone());
                            }
                            return Err(Error::Mcp {
                                server: server_name.to_string(),
                                message: "response missing both `result` and `error`".into(),
                            });
                        }
                    }
                }
                Ok(Err(e)) => {
                    return Err(Error::Mcp {
                        server: server_name.to_string(),
                        message: format!("read error: {e}"),
                    });
                }
                Err(_) => {
                    return Err(Error::Mcp {
                        server: server_name.to_string(),
                        message: format!(
                            "server timed out (no response within {}s)",
                            read_timeout.as_secs()
                        ),
                    });
                }
            }
        }
    }

    /// Read a JSON-RPC response from an SSE stream.
    async fn read_sse_response(
        client: &reqwest::Client,
        sse_url: &str,
        _post_url: &Option<String>,
        buffer: &mut String,
        expected_id: u64,
        server_name: &str,
    ) -> Result<Value> {
        // If we have buffered data, try to parse a response from it first.
        if !buffer.is_empty() {
            if let Some(result) = parse_sse_response(buffer, expected_id, server_name) {
                return result;
            }
        }

        // Otherwise, reconnect to the SSE stream and read more events.
        let response = client
            .get(sse_url)
            .header("Accept", "text/event-stream")
            .send()
            .await
            .map_err(|e| Error::Mcp {
                server: server_name.to_string(),
                message: format!("failed to reconnect to SSE endpoint `{sse_url}`: {e}"),
            })?;

        if !response.status().is_success() {
            return Err(Error::Mcp {
                server: server_name.to_string(),
                message: format!(
                    "SSE endpoint `{sse_url}` returned HTTP {}",
                    response.status()
                ),
            });
        }

        let mut stream = response.bytes_stream();

        loop {
            match timeout(Duration::from_secs(10), stream.next()).await {
                Ok(Some(Ok(chunk))) => {
                    let chunk_str = String::from_utf8_lossy(&chunk);
                    buffer.push_str(&chunk_str);

                    if let Some(result) = parse_sse_response(buffer, expected_id, server_name) {
                        return result;
                    }
                }
                Ok(Some(Err(e))) => {
                    return Err(Error::Mcp {
                        server: server_name.to_string(),
                        message: format!("error reading SSE stream from `{sse_url}`: {e}"),
                    });
                }
                Ok(None) => {
                    return Err(Error::Mcp {
                        server: server_name.to_string(),
                        message: format!("SSE stream ended without response for id {expected_id}"),
                    });
                }
                Err(_) => {
                    return Err(Error::Mcp {
                        server: server_name.to_string(),
                        message: format!("SSE timed out waiting for response for id {expected_id}"),
                    });
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// SSE parsing helpers
// ---------------------------------------------------------------------------

/// Parse an SSE stream buffer looking for an `event: endpoint` followed by
/// `data: <url>`. Returns the URL if found.
fn parse_sse_endpoint(buffer: &str) -> Option<String> {
    let mut current_event: Option<&str> = None;

    for line in buffer.lines() {
        let line = line.trim();
        if line.starts_with("event:") {
            current_event = Some(line.strip_prefix("event:").unwrap_or("").trim());
        } else if line.starts_with("data:") && current_event == Some("endpoint") {
            let data = line.strip_prefix("data:").unwrap_or("").trim();
            if !data.is_empty() {
                return Some(data.to_string());
            }
        }
        // Reset event if we see a blank line (end of event)
        if line.is_empty() {
            current_event = None;
        }
    }

    None
}

/// Parse an SSE stream buffer looking for a JSON-RPC response with the
/// given id. Returns `Some(Ok(result))` or `Some(Err(...))` if found,
/// or `None` if not yet available.
fn parse_sse_response(buffer: &str, expected_id: u64, server_name: &str) -> Option<Result<Value>> {
    let mut current_data = String::new();

    for line in buffer.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("data:") {
            let data = trimmed.strip_prefix("data:").unwrap_or("").trim();
            current_data.push_str(data);
        } else if trimmed.is_empty() && !current_data.is_empty() {
            // End of an SSE event — try to parse the accumulated data as JSON
            if let Ok(parsed) = serde_json::from_str::<Value>(&current_data) {
                if let Some(resp_id) = parsed.get("id") {
                    if resp_id.as_u64() == Some(expected_id) {
                        if let Some(err) = parsed.get("error") {
                            let msg = err
                                .get("message")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown error");
                            let code = err.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
                            return Some(Err(Error::Mcp {
                                server: server_name.to_string(),
                                message: format!("error (code {code}): {msg}"),
                            }));
                        }
                        if let Some(result) = parsed.get("result") {
                            return Some(Ok(result.clone()));
                        }
                        return Some(Err(Error::Mcp {
                            server: server_name.to_string(),
                            message: "response missing both `result` and `error`".into(),
                        }));
                    }
                }
            }
            current_data.clear();
        } else if !trimmed.starts_with("event:")
            && !trimmed.starts_with("data:")
            && !trimmed.is_empty()
        {
            // Non-data line that's not event/data — reset data accumulator
            // (SSE spec: unknown fields are ignored, but data is per-event)
            if !trimmed.starts_with(':')
                && !trimmed.starts_with("id:")
                && !trimmed.starts_with("retry:")
            {
                current_data.clear();
            }
        }
    }

    // Also check if the buffer ends with a complete JSON object (no trailing newline)
    if !current_data.is_empty() {
        if let Ok(parsed) = serde_json::from_str::<Value>(&current_data) {
            if let Some(resp_id) = parsed.get("id") {
                if resp_id.as_u64() == Some(expected_id) {
                    if let Some(err) = parsed.get("error") {
                        let msg = err
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown error");
                        let code = err.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
                        return Some(Err(Error::Mcp {
                            server: server_name.to_string(),
                            message: format!("error (code {code}): {msg}"),
                        }));
                    }
                    if let Some(result) = parsed.get("result") {
                        return Some(Ok(result.clone()));
                    }
                }
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 types for stdio MCP server mode
// ---------------------------------------------------------------------------

/// A JSON-RPC 2.0 request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

/// A JSON-RPC 2.0 response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    /// Create a successful response.
    pub fn success(id: Option<serde_json::Value>, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Create an error response.
    pub fn error(id: Option<serde_json::Value>, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }

    /// Create a method-not-found error response.
    pub fn method_not_found(id: Option<serde_json::Value>, method: &str) -> Self {
        Self::error(id, -32601, format!("Method not found: {method}"))
    }

    /// Create an internal error response.
    pub fn internal_error(id: Option<serde_json::Value>, message: impl Into<String>) -> Self {
        Self::error(id, -32603, message)
    }
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// Dispatch a JSON-RPC request to the appropriate handler.
///
/// Supports the MCP methods needed for tool proxy:
/// - `initialize` / `notifications/initialized`
/// - `tools/list` / `tools/call`
/// - `resources/list` / `resources/read`
/// - `prompts/list` / `prompts/get`
///
/// Returns a JSON-RPC response. Notifications (no `id`) return `None`.
pub async fn dispatch_request(
    request: &JsonRpcRequest,
    client: &mut McpClient,
) -> Option<JsonRpcResponse> {
    let id = request.id.clone();

    match request.method.as_str() {
        "initialize" => {
            // The client is already initialized by McpClient::spawn, so we
            // return a canned response matching what the server would expect.
            let result = serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": true,
                    "resources": true,
                    "prompts": true
                },
                "serverInfo": {
                    "name": "recursive-agent",
                    "version": "0.1.0"
                }
            });
            Some(JsonRpcResponse::success(id, result))
        }
        "notifications/initialized" => {
            // No response expected for notifications.
            None
        }
        "tools/list" => match client.list_tools().await {
            Ok(tools) => {
                let tools_arr: Vec<serde_json::Value> = tools
                    .into_iter()
                    .map(|t| {
                        serde_json::json!({
                            "name": t.name,
                            "description": t.description,
                            "inputSchema": t.input_schema,
                        })
                    })
                    .collect();
                Some(JsonRpcResponse::success(
                    id,
                    serde_json::json!({ "tools": tools_arr }),
                ))
            }
            Err(e) => Some(JsonRpcResponse::internal_error(id, e.to_string())),
        },
        "tools/call" => {
            let name = request
                .params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let arguments = request.params.get("arguments").cloned().unwrap_or_default();

            match client.call_tool(name, arguments).await {
                Ok(text) => {
                    let result = serde_json::json!({
                        "content": [{"type": "text", "text": text}]
                    });
                    Some(JsonRpcResponse::success(id, result))
                }
                Err(e) => {
                    // Return the error as a tool-level isError result, not a
                    // JSON-RPC error, so the client can handle it gracefully.
                    let result = serde_json::json!({
                        "isError": true,
                        "content": [{"type": "text", "text": e.to_string()}]
                    });
                    Some(JsonRpcResponse::success(id, result))
                }
            }
        }
        "resources/list" => match client.list_resources().await {
            Ok(resources) => {
                let resources_arr: Vec<serde_json::Value> = resources
                    .into_iter()
                    .map(|r| {
                        serde_json::json!({
                            "uri": r.uri,
                            "name": r.name,
                            "description": r.description,
                            "mimeType": r.mime_type,
                        })
                    })
                    .collect();
                Some(JsonRpcResponse::success(
                    id,
                    serde_json::json!({ "resources": resources_arr }),
                ))
            }
            Err(e) => Some(JsonRpcResponse::internal_error(id, e.to_string())),
        },
        "resources/read" => {
            let uri = request
                .params
                .get("uri")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            match client.read_resource(uri).await {
                Ok(contents) => {
                    let contents_arr: Vec<serde_json::Value> = contents
                        .into_iter()
                        .map(|c| {
                            serde_json::json!({
                                "uri": c.uri,
                                "mimeType": c.mime_type,
                                "text": c.text,
                                "blob": c.blob,
                            })
                        })
                        .collect();
                    Some(JsonRpcResponse::success(
                        id,
                        serde_json::json!({ "contents": contents_arr }),
                    ))
                }
                Err(e) => Some(JsonRpcResponse::internal_error(id, e.to_string())),
            }
        }
        "prompts/list" => match client.list_prompts().await {
            Ok(prompts) => {
                let prompts_arr: Vec<serde_json::Value> = prompts
                    .into_iter()
                    .map(|p| {
                        serde_json::json!({
                            "name": p.name,
                            "description": p.description,
                            "arguments": p.arguments,
                        })
                    })
                    .collect();
                Some(JsonRpcResponse::success(
                    id,
                    serde_json::json!({ "prompts": prompts_arr }),
                ))
            }
            Err(e) => Some(JsonRpcResponse::internal_error(id, e.to_string())),
        },
        "prompts/get" => {
            let name = request
                .params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let arguments = request
                .params
                .get("arguments")
                .and_then(|v| serde_json::from_value::<HashMap<String, String>>(v.clone()).ok());

            match client.get_prompt(name, arguments).await {
                Ok(messages) => {
                    let messages_arr: Vec<serde_json::Value> = messages
                        .into_iter()
                        .map(|m| {
                            serde_json::json!({
                                "role": m.role,
                                "content": {"type": "text", "text": m.content},
                            })
                        })
                        .collect();
                    Some(JsonRpcResponse::success(
                        id,
                        serde_json::json!({ "messages": messages_arr }),
                    ))
                }
                Err(e) => Some(JsonRpcResponse::internal_error(id, e.to_string())),
            }
        }
        _ => Some(JsonRpcResponse::method_not_found(id, &request.method)),
    }
}

fn parse_elicitation_from_error(server: &str, message: &str) -> ElicitationRequest {
    // message format: "error (code -32042): <msg>" — optional JSON data suffix
    let human = message
        .split_once(": ")
        .map(|(_, rest)| rest.to_string())
        .unwrap_or_else(|| message.to_string());
    ElicitationRequest {
        mcp_server_name: server.to_string(),
        message: human,
        mode: Some("url".into()),
        url: None,
        elicitation_id: None,
        requested_schema: None,
        title: None,
        display_name: None,
        description: None,
    }
}

// ---------------------------------------------------------------------------
// McpTool — implements the Tool trait by delegating to an McpClient
// ---------------------------------------------------------------------------

/// A tool that wraps an MCP server tool. Registered with a namespaced name
/// `mcp__<server_name>__<tool_name>`.
pub struct McpTool {
    client: Arc<Mutex<McpClient>>,
    spec: McpToolSpec,
    server_name: String,
    /// Trust level of the originating server. Controls whether annotation
    /// hints are used to derive [`ToolSideEffect`].
    trust: McpServerTrust,
}

impl McpTool {
    pub fn new(
        client: Arc<Mutex<McpClient>>,
        spec: McpToolSpec,
        server_name: impl Into<String>,
    ) -> Self {
        Self {
            client,
            spec,
            server_name: server_name.into(),
            trust: McpServerTrust::Untrusted,
        }
    }

    /// Builder: set the trust level for this tool's server.
    pub fn with_trust(mut self, trust: McpServerTrust) -> Self {
        self.trust = trust;
        self
    }
}

#[async_trait]
impl Tool for McpTool {
    fn is_deferred(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: format!("mcp__{}__{}", self.server_name, self.spec.name),
            description: format!("[mcp:{}] {}", self.server_name, self.spec.description),
            parameters: self.spec.input_schema.clone(),
        }
    }

    fn side_effect_class(&self) -> crate::tools::ToolSideEffect {
        use crate::tools::ToolSideEffect;
        // Conservative default when server is untrusted or annotations missing.
        let Some(ann) = &self.spec.annotations else {
            return ToolSideEffect::External;
        };
        if self.trust != McpServerTrust::Trusted {
            return ToolSideEffect::External;
        }
        if ann.read_only_hint {
            ToolSideEffect::ReadOnly
        } else if ann.open_world_hint {
            ToolSideEffect::External
        } else {
            ToolSideEffect::Mutating
        }
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let mut client = self.client.lock().await;
        client.call_tool(&self.spec.name, arguments).await
    }
}

// ---------------------------------------------------------------------------
// Config loading
// ---------------------------------------------------------------------------

/// Load MCP server configurations from a JSON file.
/// Expected format:
/// ```json
/// { "servers": [ { "name": "...", "command": "...", "args": [...] }, { "name": "...", "url": "http://..." } ] }
/// ```
pub fn load_mcp_config(path: &std::path::Path) -> Result<Vec<McpServer>> {
    let contents = std::fs::read_to_string(path).map_err(|e| Error::Mcp {
        server: "config".into(),
        message: format!("failed to read config `{}`: {e}", path.display()),
    })?;
    let parsed: McpConfigFile = serde_json::from_str(&contents).map_err(|e| Error::Mcp {
        server: "config".into(),
        message: format!("failed to parse config `{}`: {e}", path.display()),
    })?;
    Ok(parsed.servers)
}

#[derive(Debug, Deserialize)]
struct McpConfigFile {
    servers: Vec<McpServer>,
}

// ---------------------------------------------------------------------------
// Workspace discovery (Claude Code .mcp.json format)
// ---------------------------------------------------------------------------

/// Top-level structure of a Claude Code `.mcp.json` file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpDiscoveryFile {
    #[serde(rename = "mcpServers")]
    mcp_servers: HashMap<String, McpServerConfig>,
}

/// Discover MCP server configurations from the workspace.
///
/// Looks for (in priority order):
/// 1. `<workspace>/.mcp.json` (Claude Code format)
/// 2. `<workspace>/.recursive/mcp.json` (alternative location)
///
/// Returns an empty vec if neither file exists (not an error).
pub async fn discover_mcp_servers(workspace: &Path) -> Result<Vec<McpServer>> {
    // Priority 1: workspace root .mcp.json
    let primary = workspace.join(".mcp.json");
    if primary.exists() {
        let configs = load_mcp_discovery_config(&primary).await?;
        if !configs.is_empty() {
            return Ok(configs);
        }
    }

    // Priority 2: .recursive/mcp.json
    let fallback = workspace.join(".recursive").join("mcp.json");
    if fallback.exists() {
        let configs = load_mcp_discovery_config(&fallback).await?;
        return Ok(configs);
    }

    Ok(Vec::new())
}

/// Parse a Claude Code `.mcp.json` file into `Vec<McpServer>`.
///
/// Expected format:
/// ```json
/// {
///   "mcpServers": {
///     "server-name": {
///       "command": "path/to/server",
///       "args": ["--flag"],
///       "env": { "KEY": "value" }
///     }
///   }
/// }
/// ```
/// Or for HTTP+SSE:
/// ```json
/// {
///   "mcpServers": {
///     "server-name": {
///       "url": "http://localhost:3000/sse"
///     }
///   }
/// }
/// ```
async fn load_mcp_discovery_config(path: &Path) -> Result<Vec<McpServer>> {
    let contents = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| Error::Mcp {
            server: "discovery".into(),
            message: format!("failed to read discovery config `{}`: {e}", path.display()),
        })?;

    // Handle empty file gracefully
    if contents.trim().is_empty() {
        return Ok(Vec::new());
    }

    let parsed: McpDiscoveryFile = serde_json::from_str(&contents).map_err(|e| Error::Mcp {
        server: "discovery".into(),
        message: format!("failed to parse discovery config `{}`: {e}", path.display()),
    })?;

    let servers: Vec<McpServer> = parsed
        .mcp_servers
        .into_iter()
        .map(McpServer::from)
        .collect();

    Ok(servers)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: end-to-end tests against a mock MCP server live in
    // `tests/mcp_e2e.rs`. They cannot live here because the
    // `CARGO_BIN_EXE_mock_mcp_server` env var that resolves the mock
    // server binary's path is only injected by cargo for integration
    // tests under `tests/`, not for unit tests inside `src/`.

    #[test]
    fn load_mcp_config_parses_correctly() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{
                "servers": [
                    {"name": "fs", "command": "mcp-fs", "args": ["--root", "."]},
                    {"name": "github", "command": "mcp-gh", "args": []}
                ]
            }"#,
        )
        .unwrap();

        let servers = load_mcp_config(&path).unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "fs");
        assert_eq!(servers[0].command, "mcp-fs");
        assert_eq!(servers[0].args, vec!["--root", "."]);
        assert_eq!(servers[1].name, "github");
        assert_eq!(servers[1].command, "mcp-gh");
        assert!(servers[1].args.is_empty());
    }

    #[test]
    fn load_mcp_config_empty_servers() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("empty.json");
        std::fs::write(&path, r#"{"servers": []}"#).unwrap();

        let servers = load_mcp_config(&path).unwrap();
        assert!(servers.is_empty());
    }

    #[test]
    fn load_mcp_config_missing_file_errors() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("nonexistent.json");
        let err = load_mcp_config(&path).unwrap_err();
        assert!(err.to_string().contains("failed to read config"));
    }

    // -----------------------------------------------------------------------
    // Discovery tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn discover_finds_dot_mcp_json_in_workspace_root() {
        let dir = tempfile::TempDir::new().unwrap();
        let mcp_path = dir.path().join(".mcp.json");
        tokio::fs::write(
            &mcp_path,
            r#"{
                "mcpServers": {
                    "fs": {
                        "command": "mcp-fs",
                        "args": ["--root", "."]
                    }
                }
            }"#,
        )
        .await
        .unwrap();

        let servers = discover_mcp_servers(dir.path()).await.unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "fs");
        assert_eq!(servers[0].command, "mcp-fs");
        assert_eq!(servers[0].args, vec!["--root", "."]);
    }

    #[tokio::test]
    async fn discover_returns_empty_vec_when_no_config_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let servers = discover_mcp_servers(dir.path()).await.unwrap();
        assert!(servers.is_empty());
    }

    #[tokio::test]
    async fn discover_parses_claude_code_format_with_env() {
        let dir = tempfile::TempDir::new().unwrap();
        let mcp_path = dir.path().join(".mcp.json");
        tokio::fs::write(
            &mcp_path,
            r#"{
                "mcpServers": {
                    "github": {
                        "command": "mcp-gh",
                        "args": [],
                        "env": {
                            "GITHUB_TOKEN": "abc123"
                        }
                    },
                    "filesystem": {
                        "command": "mcp-fs",
                        "args": ["--root", "/tmp"]
                    }
                }
            }"#,
        )
        .await
        .unwrap();

        let servers = discover_mcp_servers(dir.path()).await.unwrap();
        assert_eq!(servers.len(), 2);

        let gh = servers.iter().find(|s| s.name == "github").unwrap();
        assert_eq!(gh.command, "mcp-gh");
        assert!(gh.args.is_empty());

        let fs = servers.iter().find(|s| s.name == "filesystem").unwrap();
        assert_eq!(fs.command, "mcp-fs");
        assert_eq!(fs.args, vec!["--root", "/tmp"]);
    }

    #[tokio::test]
    async fn discover_finds_dot_recursive_mcp_json_as_fallback() {
        let dir = tempfile::TempDir::new().unwrap();
        let recursive_dir = dir.path().join(".recursive");
        tokio::fs::create_dir(&recursive_dir).await.unwrap();
        let mcp_path = recursive_dir.join("mcp.json");
        tokio::fs::write(
            &mcp_path,
            r#"{
                "mcpServers": {
                    "db": {
                        "command": "mcp-db",
                        "args": ["--port", "5432"]
                    }
                }
            }"#,
        )
        .await
        .unwrap();

        let servers = discover_mcp_servers(dir.path()).await.unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "db");
        assert_eq!(servers[0].command, "mcp-db");
    }

    #[tokio::test]
    async fn discover_dot_mcp_json_takes_priority_over_dot_recursive() {
        let dir = tempfile::TempDir::new().unwrap();

        // Primary .mcp.json
        tokio::fs::write(
            dir.path().join(".mcp.json"),
            r#"{
                "mcpServers": {
                    "primary": {
                        "command": "primary-server",
                        "args": []
                    }
                }
            }"#,
        )
        .await
        .unwrap();

        // Fallback .recursive/mcp.json
        let recursive_dir = dir.path().join(".recursive");
        tokio::fs::create_dir(&recursive_dir).await.unwrap();
        tokio::fs::write(
            recursive_dir.join("mcp.json"),
            r#"{
                "mcpServers": {
                    "fallback": {
                        "command": "fallback-server",
                        "args": []
                    }
                }
            }"#,
        )
        .await
        .unwrap();

        let servers = discover_mcp_servers(dir.path()).await.unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "primary");
    }

    #[tokio::test]
    async fn discover_malformed_json_returns_descriptive_error() {
        let dir = tempfile::TempDir::new().unwrap();
        tokio::fs::write(dir.path().join(".mcp.json"), "not valid json")
            .await
            .unwrap();

        let err = discover_mcp_servers(dir.path()).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("failed to parse discovery config"),
            "error should mention parsing failure: {msg}"
        );
    }

    #[tokio::test]
    async fn discover_empty_file_returns_empty_vec() {
        let dir = tempfile::TempDir::new().unwrap();
        tokio::fs::write(dir.path().join(".mcp.json"), "")
            .await
            .unwrap();

        let servers = discover_mcp_servers(dir.path()).await.unwrap();
        assert!(servers.is_empty());
    }

    #[tokio::test]
    async fn discover_empty_mcp_servers_object_returns_empty_vec() {
        let dir = tempfile::TempDir::new().unwrap();
        tokio::fs::write(dir.path().join(".mcp.json"), r#"{"mcpServers": {}}"#)
            .await
            .unwrap();

        let servers = discover_mcp_servers(dir.path()).await.unwrap();
        assert!(servers.is_empty());
    }

    // -----------------------------------------------------------------------
    // HTTP+SSE transport tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_http_sse_discovery_with_url() {
        let dir = tempfile::TempDir::new().unwrap();
        let mcp_path = dir.path().join(".mcp.json");
        tokio::fs::write(
            &mcp_path,
            r#"{
                "mcpServers": {
                    "remote": {
                        "url": "http://example.com/sse"
                    }
                }
            }"#,
        )
        .await
        .unwrap();

        let servers = discover_mcp_servers(dir.path()).await.unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "remote");
        assert_eq!(servers[0].command, "");
        assert!(servers[0].args.is_empty());
        assert_eq!(servers[0].url.as_deref(), Some("http://example.com/sse"));
    }

    #[test]
    fn test_parse_sse_endpoint() {
        let buffer = "event: endpoint\ndata: http://localhost:3000/message\n\n";
        assert_eq!(
            parse_sse_endpoint(buffer),
            Some("http://localhost:3000/message".to_string())
        );

        // No endpoint event
        let buffer = "event: message\ndata: {\"key\": \"value\"}\n\n";
        assert_eq!(parse_sse_endpoint(buffer), None);

        // Empty data
        let buffer = "event: endpoint\ndata: \n\n";
        assert_eq!(parse_sse_endpoint(buffer), None);
    }

    #[test]
    fn test_parse_sse_response() {
        let buffer = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"key\":\"value\"}}\n\n";
        let result = parse_sse_response(buffer, 1, "test");
        assert!(result.is_some());
        let result = result.unwrap().unwrap();
        assert_eq!(result.get("key").and_then(|v| v.as_str()), Some("value"));

        // Wrong id
        let result = parse_sse_response(buffer, 2, "test");
        assert!(result.is_none());

        // Error response
        let buffer =
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32601,\"message\":\"Method not found\"}}\n\n";
        let result = parse_sse_response(buffer, 1, "test");
        assert!(result.is_some());
        assert!(result.unwrap().is_err());
    }

    #[test]
    fn test_parse_sse_response_multiline_data() {
        let buffer =
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\"\ndata: :{\"key\":\"value\"}}\n\n";
        let result = parse_sse_response(buffer, 1, "test");
        assert!(result.is_some());
        let result = result.unwrap().unwrap();
        assert_eq!(result.get("key").and_then(|v| v.as_str()), Some("value"));
    }

    #[test]
    fn test_parse_sse_response_empty_data_lines() {
        // Empty data: lines interspersed — they should be skipped (contribute nothing)
        // and the valid JSON line should still be parsed correctly.
        let buffer =
            "data: \ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\ndata: \n\n";
        let result = parse_sse_response(buffer, 1, "test");
        assert!(result.is_some());
        let result = result.unwrap().unwrap();
        assert_eq!(result.get("ok").and_then(|v| v.as_bool()), Some(true));
    }

    #[test]
    fn test_parse_sse_response_invalid_json() {
        // data line with completely invalid JSON — should not panic, should return None
        // (the parse fails silently and current_data is cleared on the blank line).
        let buffer = "data: not-json-at-all\n\n";
        let result = parse_sse_response(buffer, 1, "test");
        // No valid JSON-RPC response could be extracted → None
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_sse_response_no_data_prefix() {
        // Lines without `data:` prefix should be ignored entirely.
        let buffer = "some random line\nanother line\n\n";
        let result = parse_sse_response(buffer, 1, "test");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_http_sse_config_url_takes_priority() {
        // When both `command` and `url` are set in the config, the url-based
        // transport should be selected (url takes priority per spawn logic).
        let dir = tempfile::TempDir::new().unwrap();
        let mcp_path = dir.path().join(".mcp.json");
        tokio::fs::write(
            &mcp_path,
            r#"{
                "mcpServers": {
                    "hybrid": {
                        "command": "some-binary",
                        "args": ["--flag"],
                        "url": "http://example.com/sse"
                    }
                }
            }"#,
        )
        .await
        .unwrap();

        let servers = discover_mcp_servers(dir.path()).await.unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "hybrid");
        // url is present → spawn will choose HTTP+SSE transport
        assert_eq!(servers[0].url.as_deref(), Some("http://example.com/sse"));
        // command is also stored but url takes priority in spawn()
        assert_eq!(servers[0].command, "some-binary");

        // Verify the spawn logic: url.is_some() → spawn_http_sse path
        let server = &servers[0];
        assert!(
            server.url.is_some(),
            "url should be present, meaning HTTP+SSE transport is selected"
        );
    }

    #[test]
    fn test_load_mcp_config_with_url() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{
                "servers": [
                    {"name": "local", "command": "mcp-fs", "args": ["--root", "."]},
                    {"name": "remote", "url": "http://localhost:3000/sse"}
                ]
            }"#,
        )
        .unwrap();

        let servers = load_mcp_config(&path).unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "local");
        assert_eq!(servers[0].command, "mcp-fs");
        assert!(servers[0].url.is_none());
        assert_eq!(servers[1].name, "remote");
        assert_eq!(servers[1].command, "");
        assert_eq!(servers[1].url.as_deref(), Some("http://localhost:3000/sse"));
    }

    // ── parse_sse_endpoint edge cases ───────────────────────────────────────

    #[test]
    fn parse_sse_endpoint_data_before_event_header_returns_none() {
        // data: line before event: endpoint must NOT return Some
        // (kills && → || mutant at line 1000)
        let buffer = "data: http://localhost:3000/message\nevent: endpoint\n\n";
        // The data: line arrived before event: endpoint was seen → should not match
        // (endpoint event must precede its data:)
        // Note: SSE spec says event type applies to the whole event block; our parser
        // sets current_event before the data: check. So this test verifies the
        // "event before data" invariant within a single event block.
        let result = parse_sse_endpoint(buffer);
        // With && → || mutant: data before event triggers match → Some("http://...")
        // With correct &&: current_event is None when data: is seen → no match
        assert!(
            result.is_none(),
            "data: before event: endpoint must not match; got {result:?}"
        );
    }

    #[test]
    fn parse_sse_endpoint_non_endpoint_event_ignored() {
        // event: other should NOT trigger data: as endpoint
        // (kills == → != mutant at line 1000)
        let buffer = "event: message\ndata: http://localhost:3000/endpoint-data\n\n";
        assert_eq!(
            parse_sse_endpoint(buffer),
            None,
            "event: message must not expose data: as endpoint"
        );
    }

    #[test]
    fn parse_sse_endpoint_event_then_empty_data_returns_none() {
        // data: with whitespace-only content must not return Some
        // (kills delete ! in is_empty check)
        let buffer = "event: endpoint\ndata:    \n\n";
        assert_eq!(
            parse_sse_endpoint(buffer),
            None,
            "empty data after endpoint event must return None"
        );
    }

    // ── parse_sse_response edge cases ──────────────────────────────────────

    #[test]
    fn parse_sse_response_error_code_minus_one_when_absent() {
        // Error response missing "code" field → should use -1, not 1
        // (kills delete - mutant at line 1036)
        let buffer = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"message\":\"oops\"}}\n\n";
        let result = parse_sse_response(buffer, 1, "srv");
        assert!(result.is_some(), "must parse error response");
        let err_msg = result.unwrap().unwrap_err().to_string();
        assert!(
            err_msg.contains("-1"),
            "missing code should default to -1; got: {err_msg}"
        );
    }

    #[test]
    fn parse_sse_response_no_trailing_newline_still_parses() {
        // Buffer without trailing \n — uses the fallback path (line 1069)
        // (kills delete ! in the if !current_data.is_empty() at 1069)
        let buffer = "data: {\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{\"v\":42}}";
        let result = parse_sse_response(buffer, 5, "srv");
        assert!(result.is_some(), "no trailing newline must still parse");
        let val = result.unwrap().unwrap();
        assert_eq!(val.get("v").and_then(|v| v.as_i64()), Some(42));
    }

    #[test]
    fn parse_sse_response_id_field_mismatch_returns_none() {
        // id in response != expected_id → None
        // (kills == → != mutant at line 1030 and 1072)
        let buffer = "data: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"ok\":true}}\n\n";
        assert!(
            parse_sse_response(buffer, 99, "srv").is_none(),
            "wrong id must return None"
        );
        // Right id must return Some
        assert!(
            parse_sse_response(buffer, 3, "srv").is_some(),
            "correct id must return Some"
        );
    }

    #[test]
    fn parse_sse_response_sse_field_prefix_lines_do_not_reset_data() {
        // Lines starting with ':', id:, retry: must NOT clear data accumulator
        // (kills the && → || mutations at lines 1059-1061)
        let buffer = "data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\"\n: comment ignored\ndata: :{\"v\":7}}\n\n";
        let result = parse_sse_response(buffer, 7, "srv");
        assert!(
            result.is_some(),
            "comment line must not reset data accumulator"
        );
    }

    #[test]
    fn parse_sse_response_unknown_line_resets_data_accumulator() {
        // An unrecognised field (not event:, data:, id:, retry:, :) should clear
        // the data accumulator, causing the JSON to not parse.
        // (kills the delete ! mutations at lines 1053-1055)
        let buffer =
            "data: {\"jsonrpc\":\"2.0\",\"id\":8\nunknown: stuff\ndata: ,\"result\":{}}\n\n";
        // After "unknown: stuff" resets data, the next data: fragment is incomplete JSON
        let result = parse_sse_response(buffer, 8, "srv");
        // Incomplete/invalid JSON after reset → None
        assert!(
            result.is_none(),
            "unknown SSE field must reset accumulator; incomplete JSON should yield None"
        );
    }

    // ── JsonRpcResponse builder tests ─────────────────────────────────────────

    #[test]
    fn method_not_found_uses_negative_error_code() {
        // kills `delete - in JsonRpcResponse::method_not_found` (line 1149)
        // Mutant changes -32601 → 32601; this test catches it.
        let resp = JsonRpcResponse::method_not_found(Some(serde_json::json!(1)), "unknown_method");
        let err_obj = resp
            .error
            .expect("method_not_found must set the error field");
        assert_eq!(
            err_obj.code, -32601,
            "method_not_found must use the JSON-RPC -32601 error code (negative)"
        );
        assert!(
            err_obj.message.contains("unknown_method"),
            "error message must include the method name: {}",
            err_obj.message
        );
    }

    #[test]
    fn internal_error_uses_negative_error_code() {
        // kills `delete - in JsonRpcResponse::internal_error` (analogous to line 1149)
        let resp = JsonRpcResponse::internal_error(None, "something failed");
        let err_obj = resp.error.expect("internal_error must set the error field");
        assert_eq!(
            err_obj.code, -32603,
            "internal_error must use the JSON-RPC -32603 error code (negative)"
        );
    }
}
