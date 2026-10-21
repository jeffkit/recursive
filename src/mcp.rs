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
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;
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
    /// URL for HTTP transport (mutually exclusive with `command`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Extra HTTP headers sent with every request to a remote (`url`)
    /// server — the auth channel for SaaS servers (GitHub, Notion, …).
    /// Values support `${VAR}` env expansion, so a token can live in the
    /// environment rather than the checked-in `.mcp.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    /// Remote transport override: `"sse"` (legacy HTTP+SSE) or `"http"`
    /// (2025-03-26 Streamable HTTP). Absent = auto (Streamable HTTP with
    /// fallback to legacy SSE). Ignored for stdio servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
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
    /// URL for HTTP transport, or None if using stdio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Extra environment variables to set when spawning a stdio subprocess.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    /// Extra HTTP headers for a remote server (auth). See
    /// [`McpServerConfig::headers`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    /// Remote transport override (`"sse"` / `"http"`). See
    /// [`McpServerConfig::transport`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
}

impl From<(String, McpServerConfig)> for McpServer {
    fn from((name, config): (String, McpServerConfig)) -> Self {
        Self {
            name,
            command: config.command,
            args: config.args,
            url: config.url,
            env: config.env,
            headers: config.headers,
            transport: config.transport,
        }
    }
}

/// A remote MCP transport selected for a `url`-based server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteTransport {
    /// Legacy HTTP+SSE (`GET` stream + `POST` endpoint discovered from it).
    LegacySse,
    /// 2025-03-26 Streamable HTTP (single endpoint, `POST` JSON-RPC).
    StreamableHttp,
    /// Try Streamable HTTP first, fall back to legacy SSE.
    Auto,
}

/// Resolve the configured `transport` string to a [`RemoteTransport`].
/// Unknown values warn and fall back to auto so a typo degrades to the
/// working transport instead of failing the server outright.
fn resolve_remote_transport(transport: Option<&str>, server: &str) -> RemoteTransport {
    match transport.map(str::trim) {
        None | Some("") | Some("auto") => RemoteTransport::Auto,
        Some("sse") => RemoteTransport::LegacySse,
        Some("http") | Some("streamable") | Some("streamable-http") => {
            RemoteTransport::StreamableHttp
        }
        Some(other) => {
            tracing::warn!(
                target: "recursive::mcp",
                server = %server,
                transport = %other,
                "unknown MCP transport; falling back to auto"
            );
            RemoteTransport::Auto
        }
    }
}

/// Whether a server advertised a capability (issue #181).
///
/// The MCP spec represents a capability as an **object** — `"tools": {}`,
/// `"resources": {"subscribe": true}` — optionally with sub-capabilities.
/// Older/looser servers use bare `true`. Reading the field with
/// `as_bool()` alone therefore reported every spec-shaped server as "does
/// not advertise". A bare array is accepted as well, since some servers
/// publish capability lists inline.
///
/// `false`, explicit `null`, and an absent field all mean "not
/// advertised".
fn has_capability(value: Option<&Value>) -> bool {
    matches!(
        value,
        Some(Value::Bool(true)) | Some(Value::Object(_)) | Some(Value::Array(_))
    )
}

/// Expand `${VAR}` references in a config string against the process
/// environment (Claude Code `.mcp.json` convention). An unset variable
/// expands to the empty string; `$${` is not special (only `${…}`).
fn expand_env(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                out.push_str(&std::env::var(name).unwrap_or_default());
                rest = &after[end + 1..];
            }
            None => {
                // Unterminated `${` — keep it literally.
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
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

/// Response slots shared between an HTTP+SSE client and its resident
/// reader task, keyed by JSON-RPC request id (issue #178).
type PendingResponses = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

/// The underlying transport for an MCP client.
#[allow(clippy::large_enum_variant)]
enum McpTransport {
    /// Stdio subprocess transport.
    Stdio {
        stdin: ChildStdin,
        reader: BufReader<ChildStdout>,
        child: Option<Child>,
    },
    /// Legacy HTTP+SSE transport (2024-11-05): a single long-lived `GET`
    /// event stream carries every server→client message, while
    /// client→server messages are `POST`ed to the endpoint the stream
    /// announces (issue #178).
    HttpSse {
        client: reqwest::Client,
        /// The SSE endpoint URL (the long-lived `GET`).
        sse_url: String,
        /// Messenger endpoint, resolved to an absolute URL from the
        /// `endpoint` event's `data:` payload — a relative payload is
        /// joined against [`sse_url`](Self::HttpSse::sse_url).
        post_url: String,
        /// Sending halves of the in-flight response slots, shared with the
        /// reader task so it can wake the right waiter by id.
        pending: PendingResponses,
        /// Receiving halves of [`pending`](Self::HttpSse::pending), owned
        /// by this client and consumed by `read_response`.
        waiters: HashMap<u64, oneshot::Receiver<Result<Value>>>,
        /// The resident stream reader. Bound to the client: aborted on
        /// `Drop` so the session's stream is not left behind.
        reader: JoinHandle<()>,
    },

    /// 2025-03-26 Streamable HTTP transport: a single endpoint that
    /// accepts JSON-RPC via `POST` and answers either with a JSON body or
    /// with an SSE stream in the POST response.
    StreamableHttp {
        client: reqwest::Client,
        /// Single endpoint URL (both POST and, if needed, GET).
        url: String,
        /// Session id returned by the server (the `Mcp-Session-Id` header
        /// from the initialize response), echoed on subsequent requests.
        session_id: Option<String>,
        /// Response body parsed from the last POST, consumed by
        /// [`McpClient::read_response`].
        pending: Option<Value>,
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
            Self::StreamableHttp {
                url, session_id, ..
            } => f
                .debug_struct("StreamableHttp")
                .field("url", url)
                .field("session_id", session_id)
                .finish(),
        }
    }
}

pub use crate::tools::elicitation::{
    new_elicitation_slot, ElicitationHandler, ElicitationRequest, SharedElicitationHandler,
};

/// An MCP client owns a transport and manages JSON-RPC communication.
pub struct McpClient {
    transport: McpTransport,
    next_id: u64,
    /// Capabilities advertised by the server during initialization.
    capabilities: ServerCapabilities,
    /// Name of the MCP server (for error reporting).
    server_name: String,
    /// How long to wait for a single line of stdio output — or for a
    /// single HTTP response — before declaring the server hung. Applies
    /// to the `initialize` handshake and the cheap request/response
    /// methods (`tools/list`, …). Defaults to
    /// [`DEFAULT_STDIO_READ_TIMEOUT`]; tests use a shorter value to keep
    /// the suite fast.
    ///
    /// On the HTTP transports it is the per-request budget for every
    /// request except `tools/call` (see
    /// [`McpClient::http_request_timeout`]).
    read_timeout: Duration,
    /// How long to wait for a `tools/call` to produce its response
    /// (issue #181). Kept separate from [`McpClient::read_timeout`] on
    /// purpose: a build/retrieval/external API tool routinely takes
    /// minutes, while a hung handshake must still be reported in
    /// seconds. Defaults to [`DEFAULT_MCP_TOOL_TIMEOUT`]; override with
    /// [`McpClient::with_tool_timeout`] or the
    /// `RECURSIVE_MCP_TOOL_TIMEOUT_SECS` environment variable.
    ///
    /// Applies to both transports: per-line idle budget for stdio, and a
    /// reqwest per-request timeout on HTTP.
    tool_timeout: Duration,
    /// Optional host elicitation handler (Claude control channel).
    elicitation: Option<SharedElicitationHandler>,
}

/// Default timeout for reading a single JSON-RPC line from a stdio
/// MCP server. Generous on purpose — real servers can pause briefly
/// during large tool runs without being broken.
pub const DEFAULT_STDIO_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Default timeout for a single `tools/call` on a stdio MCP server
/// (issue #181). Long-running MCP tools — builds, retrieval, external
/// APIs — legitimately take minutes, so this is deliberately far larger
/// than [`DEFAULT_STDIO_READ_TIMEOUT`], which still governs the
/// handshake and the discovery calls.
pub const DEFAULT_MCP_TOOL_TIMEOUT: Duration = Duration::from_secs(300);

/// Fixed, bounded client-level timeout for the reqwest clients used by the
/// HTTP transports (issue #181 review). It is the transport-wide backstop
/// that every per-request budget in [`McpClient::http_request_timeout`]
/// overrides; it deliberately does **not** follow
/// [`DEFAULT_MCP_TOOL_TIMEOUT`], so a remote server that never answers is
/// reported in 30s rather than in 5 minutes.
pub const HTTP_DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Environment variable that overrides [`DEFAULT_MCP_TOOL_TIMEOUT`] for
/// every client built by [`McpClient::spawn`]. An unparseable value (or
/// `0`) is ignored in favour of the default.
pub const MCP_TOOL_TIMEOUT_ENV: &str = "RECURSIVE_MCP_TOOL_TIMEOUT_SECS";

/// Resolve the configured `tools/call` timeout for a newly spawned
/// stdio client: `RECURSIVE_MCP_TOOL_TIMEOUT_SECS` when set to a usable
/// whole number of seconds, otherwise [`DEFAULT_MCP_TOOL_TIMEOUT`].
fn default_tool_timeout() -> Duration {
    match std::env::var(MCP_TOOL_TIMEOUT_ENV) {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(secs) if secs > 0 => Duration::from_secs(secs),
            _ => {
                tracing::warn!(
                    target: "recursive::mcp",
                    value = %raw,
                    env = MCP_TOOL_TIMEOUT_ENV,
                    default_secs = DEFAULT_MCP_TOOL_TIMEOUT.as_secs(),
                    "ignoring unusable MCP tool timeout override"
                );
                DEFAULT_MCP_TOOL_TIMEOUT
            }
        },
        Err(_) => DEFAULT_MCP_TOOL_TIMEOUT,
    }
}

/// Protocol revision this client offers during `initialize`. The server may
/// negotiate down; [`SUPPORTED_PROTOCOL_VERSIONS`] lists what we can speak.
pub const MCP_PROTOCOL_VERSION: &str = "2025-03-26";

/// MCP protocol revisions this client implements, newest first.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-03-26", "2024-11-05"];

/// Default cap on the bytes an MCP tool result may contribute to the
/// transcript before it is truncated (see [`McpTool`]). Mirrors
/// `web_fetch`'s default `max_bytes`.
pub const DEFAULT_MCP_MAX_OUTPUT_BYTES: usize = 65536;

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
        match &mut self.transport {
            McpTransport::Stdio { child, .. } => {
                if let Some(mut child) = child.take() {
                    let _ = child.start_kill();
                }
            }
            // The reader owns the session's long-lived GET: aborting it
            // closes the stream instead of leaving an orphan task and a
            // socket behind (issue #178).
            McpTransport::HttpSse { reader, .. } => reader.abort(),
            McpTransport::StreamableHttp { .. } => {}
        }
    }
}

impl McpClient {
    /// Spawn an MCP server (stdio or HTTP+SSE), perform the initialize
    /// handshake, and return a ready-to-use client.
    ///
    /// If `server.url` is set, uses HTTP+SSE transport. Otherwise uses stdio.
    pub async fn spawn(server: &McpServer) -> Result<Self> {
        Self::spawn_with_timeout_and_tool_timeout(
            server,
            DEFAULT_STDIO_READ_TIMEOUT,
            default_tool_timeout(),
        )
        .await
    }

    /// Like [`spawn`], but with a custom read timeout. Tests use a short
    /// value (e.g. 500ms) to keep the suite fast; production callers
    /// should stick to [`spawn`].
    ///
    /// For stdio this is the per-line budget; for the HTTP transports it
    /// is the per-request budget for everything except `tools/call`
    /// (issue #181 review).
    pub async fn spawn_with_timeout(server: &McpServer, read_timeout: Duration) -> Result<Self> {
        Self::spawn_with_timeout_and_tool_timeout(server, read_timeout, default_tool_timeout())
            .await
    }

    /// The full form of [`spawn`] (issue #181): the short `read_timeout`
    /// governs the handshake and discovery, while `tool_timeout` governs
    /// each `tools/call`. Applies to both transports.
    async fn spawn_with_timeout_and_tool_timeout(
        server: &McpServer,
        read_timeout: Duration,
        tool_timeout: Duration,
    ) -> Result<Self> {
        let Some(url) = &server.url else {
            return Self::spawn_stdio(server, read_timeout, tool_timeout).await;
        };
        // Remote transports apply both budgets per request
        // (`http_request_timeout`); the reqwest client itself keeps a
        // fixed, bounded backstop so a dead server never waits for
        // `tool_timeout` (issue #181 review, spec 陷阱 #1).
        match resolve_remote_transport(server.transport.as_deref(), &server.name) {
            RemoteTransport::LegacySse => {
                Self::spawn_http_sse(server, url, read_timeout, tool_timeout).await
            }
            RemoteTransport::StreamableHttp => {
                Self::spawn_streamable(server, url, read_timeout, tool_timeout).await
            }
            RemoteTransport::Auto => {
                match Self::spawn_streamable(server, url, read_timeout, tool_timeout).await {
                    Ok(client) => Ok(client),
                    Err(streamable_err) => {
                        tracing::debug!(
                            target: "recursive::mcp",
                            server = %server.name,
                            error = %streamable_err,
                            "Streamable HTTP unavailable; falling back to legacy SSE"
                        );
                        Self::spawn_http_sse(server, url, read_timeout, tool_timeout).await
                    }
                }
            }
        }
    }

    /// Build the shared HTTP client for a remote server, applying its
    /// configured `headers` (auth). Header values support `${VAR}` env
    /// expansion so tokens need not be stored in the config file.
    ///
    /// The client-level timeout stays a fixed, bounded value — it is the
    /// transport-wide backstop, NOT the `tools/call` budget. Putting
    /// `tool_timeout` here would widen the `initialize` handshake too and
    /// delay a "server never started" failure to the full 300s default
    /// (issue #181 review, spec 陷阱 #1). The per-call budget is applied
    /// per request by [`McpClient::http_request_timeout`] instead.
    fn build_http_client(server: &McpServer) -> Result<reqwest::Client> {
        Self::build_http_client_with_total_timeout(server, Some(HTTP_DEFAULT_REQUEST_TIMEOUT))
    }

    /// [`McpClient::build_http_client`] with an explicit transport-wide
    /// timeout.
    ///
    /// The legacy SSE stream passes `None` (issue #178): reqwest's
    /// client-level timeout covers *reading the body*, so the fixed 30s
    /// budget would terminate a healthy long-lived event stream at 30s.
    /// Every wait on that transport is still bounded elsewhere — the
    /// handshake `GET` by a `tokio::time::timeout`, each `POST` by its
    /// per-request `.timeout()`, and each response by
    /// `read_response`'s budget.
    fn build_http_client_with_total_timeout(
        server: &McpServer,
        total_timeout: Option<Duration>,
    ) -> Result<reqwest::Client> {
        let mut default_headers = reqwest::header::HeaderMap::new();
        if let Some(extra) = &server.headers {
            for (key, value) in extra {
                let name =
                    reqwest::header::HeaderName::from_bytes(key.as_bytes()).map_err(|e| {
                        Error::Mcp {
                            server: server.name.clone(),
                            message: format!("invalid header name `{key}`: {e}"),
                        }
                    })?;
                let expanded = expand_env(value);
                let value =
                    reqwest::header::HeaderValue::from_str(&expanded).map_err(|e| Error::Mcp {
                        server: server.name.clone(),
                        message: format!("invalid header value for `{key}`: {e}"),
                    })?;
                default_headers.insert(name, value);
            }
        }
        let mut builder = reqwest::Client::builder().default_headers(default_headers);
        if let Some(total_timeout) = total_timeout {
            builder = builder.timeout(total_timeout);
        }
        builder.build().map_err(|e| Error::Mcp {
            server: server.name.clone(),
            message: format!("failed to build HTTP client: {e}"),
        })
    }

    /// Budget for a single HTTP request (issue #181 review).
    ///
    /// Tiering matches stdio: only `tools/call` gets the configurable
    /// [`McpClient::tool_timeout`]; every other request — the
    /// `initialize` handshake above all — is bounded by the short
    /// [`McpClient::read_timeout`]. This keeps `with_tool_timeout`
    /// meaningful on the HTTP transports too (it is no longer a silent
    /// no-op) while a dead remote server is still reported in seconds,
    /// not in `tool_timeout`.
    ///
    /// The client-level [`HTTP_DEFAULT_REQUEST_TIMEOUT`] remains a fixed
    /// transport-wide backstop and is *not* widened by `tool_timeout`.
    fn http_request_timeout(&self, request: &Value) -> Duration {
        if request.get("method").and_then(|m| m.as_str()) == Some("tools/call") {
            self.tool_timeout
        } else {
            self.read_timeout
        }
    }

    /// Spawn via stdio subprocess.
    async fn spawn_stdio(
        server: &McpServer,
        read_timeout: Duration,
        tool_timeout: Duration,
    ) -> Result<Self> {
        let mut cmd = Command::new(&server.command);
        cmd.args(&server.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        if let Some(env) = &server.env {
            cmd.envs(env);
        }
        // Goal 412: the server leads its own process group, so the graceful
        // shutdown below signals the server *and* the wrappers it spawned
        // (a `npx`/`uvx` launcher leaves a real server as its child).
        let mut child = crate::process::spawn_grouped(&mut cmd).map_err(|e| Error::Mcp {
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
            tool_timeout,
            elicitation: None,
        };

        client.do_initialize(&server.name).await?;
        Ok(client)
    }

    /// Spawn via the 2025-03-26 Streamable HTTP transport.
    ///
    /// Unlike legacy SSE there is no separate event-stream connection: every
    /// JSON-RPC message is `POST`ed to the single `url`, and the server
    /// answers either with a JSON body or an SSE stream in that same POST
    /// response. The `initialize` handshake establishes the optional
    /// `Mcp-Session-Id`.
    async fn spawn_streamable(
        server: &McpServer,
        url: &str,
        read_timeout: Duration,
        tool_timeout: Duration,
    ) -> Result<Self> {
        let http_client = Self::build_http_client(server)?;
        let mut client = Self {
            transport: McpTransport::StreamableHttp {
                client: http_client,
                url: url.to_string(),
                session_id: None,
                pending: None,
            },
            next_id: 1,
            capabilities: ServerCapabilities::default(),
            server_name: server.name.clone(),
            read_timeout,
            tool_timeout,
            elicitation: None,
        };
        client.do_initialize(&server.name).await?;
        Ok(client)
    }

    /// Spawn via the legacy HTTP+SSE transport (2024-11-05 revision).
    ///
    /// One long-lived `GET` stream serves the whole session (issue #178):
    /// a resident reader task consumes it and dispatches every JSON-RPC
    /// message by id, while requests go out as `POST`s to the endpoint the
    /// stream announces. The previous implementation re-`GET` the stream
    /// per response, which made the server open a *new* session each time —
    /// replies for the original `sessionId` never arrived on the new
    /// stream.
    async fn spawn_http_sse(
        server: &McpServer,
        url: &str,
        read_timeout: Duration,
        tool_timeout: Duration,
    ) -> Result<Self> {
        // No client-level timeout: the GET body is the session itself (see
        // `build_http_client_with_total_timeout`).
        let http_client = Self::build_http_client_with_total_timeout(server, None)?;

        // Connect to the SSE endpoint and read the initial event to
        // discover the message endpoint. The header exchange is bounded by
        // the short read budget, not by a 30s client timeout.
        let response = timeout(
            read_timeout,
            http_client
                .get(url)
                .header("Accept", "text/event-stream")
                .send(),
        )
        .await
        .map_err(|_| Error::Mcp {
            server: server.name.clone(),
            message: format!("timeout connecting to SSE endpoint `{url}`"),
        })?
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

        // A non-stream body cannot carry JSON-RPC replies; say so instead of
        // silently waiting for events that will never arrive. The check is
        // deliberately lenient about parameters (`text/event-stream;
        // charset=utf-8`).
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !content_type.contains("text/event-stream") {
            return Err(Error::Mcp {
                server: server.name.clone(),
                message: format!(
                    "SSE endpoint `{url}` returned Content-Type `{content_type}`, \
                     expected `text/event-stream`"
                ),
            });
        }

        let mut stream = response.bytes_stream();
        // Raw bytes: whatever the handshake consumes is replayed into the
        // reader, so an event that shares a chunk with `endpoint` is not
        // lost (and a multi-byte sequence split across chunks is not
        // mangled by a per-chunk lossy conversion).
        let mut handshake: Vec<u8> = Vec::new();
        let mut found_endpoint: Option<String> = None;

        // Read up to 64KB to find the endpoint event.
        let max_read = 65536;

        while handshake.len() < max_read {
            match timeout(read_timeout, stream.next()).await {
                Ok(Some(Ok(chunk))) => {
                    handshake.extend_from_slice(&chunk);
                    if let Some(endpoint) = parse_sse_endpoint(&String::from_utf8_lossy(&handshake))
                    {
                        found_endpoint = Some(endpoint);
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

        let Some(raw_endpoint) = found_endpoint else {
            let received = String::from_utf8_lossy(&handshake);
            return Err(Error::Mcp {
                server: server.name.clone(),
                message: format!(
                    "SSE endpoint `{url}` did not send an `endpoint` event. Received data: {}",
                    &received[..received.len().min(200)]
                ),
            });
        };

        // The endpoint is usually a relative path (`/messages?sessionId=…`);
        // POSTing it verbatim is an invalid URL (issue #178).
        let post_url = resolve_sse_endpoint(url, &raw_endpoint, &server.name)?;

        // Start the resident reader before the handshake POST: the
        // `initialize` reply arrives on this stream, not in the POST body.
        let pending: PendingResponses = Arc::default();
        let reader = spawn_sse_reader(
            stream,
            &handshake,
            pending.clone(),
            http_client.clone(),
            post_url.clone(),
            server.name.clone(),
        );

        let mut client = Self {
            transport: McpTransport::HttpSse {
                client: http_client,
                sse_url: url.to_string(),
                post_url,
                pending,
                waiters: HashMap::new(),
                reader,
            },
            next_id: 1,
            capabilities: ServerCapabilities::default(),
            server_name: server.name.clone(),
            read_timeout,
            tool_timeout,
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
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "recursive-agent",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            )
            .await?;

        // Check protocol version in response. A server may legitimately
        // answer with an older revision it prefers; only warn when it picks
        // one this client does not implement at all.
        if let Some(server_proto) = init_result.get("protocolVersion").and_then(|v| v.as_str()) {
            if !SUPPORTED_PROTOCOL_VERSIONS.contains(&server_proto) {
                tracing::warn!(
                    target: "recursive::mcp",
                    server = %server_name,
                    server_protocol = %server_proto,
                    "MCP server protocol version mismatch"
                );
            }
        }

        // Parse capabilities from the server response. The MCP spec
        // advertises a capability as an *object* (optionally carrying
        // sub-capabilities); some servers use the legacy boolean form.
        // `as_bool()` alone therefore reported every spec-shaped server
        // as "does not advertise" (issue #181).
        if let Some(caps) = init_result.get("capabilities") {
            self.capabilities.tools = has_capability(caps.get("tools"));
            self.capabilities.resources = has_capability(caps.get("resources"));
            self.capabilities.prompts = has_capability(caps.get("prompts"));
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

    /// Override the per-`tools/call` budget (issue #181).
    ///
    /// The handshake and `tools/list` keep using the short
    /// [`read_timeout`](Self::spawn_with_timeout) — deliberately, so a
    /// server that never started is still reported in seconds — while a
    /// tool invocation gets this budget instead. Defaults to
    /// [`DEFAULT_MCP_TOOL_TIMEOUT`] (300s), or
    /// `RECURSIVE_MCP_TOOL_TIMEOUT_SECS` when set.
    ///
    /// Applies to **both** transports (issue #181 review): a per-line
    /// idle budget on stdio, and a reqwest per-request timeout on
    /// streamable HTTP / HTTP+SSE. It is not stdio-only.
    pub fn with_tool_timeout(mut self, tool_timeout: Duration) -> Self {
        self.tool_timeout = tool_timeout;
        self
    }

    /// Call a tool by name with the given arguments.
    /// Returns the textual content of the first `content[].text` block.
    /// Errors if `isError` is true in the response.
    ///
    /// Uses the per-call tool timeout
    /// ([`with_tool_timeout`](Self::with_tool_timeout)) rather than the
    /// short read timeout, so long-running tools are not cut off at 10s
    /// (issue #181).
    ///
    /// When the server returns JSON-RPC `-32042` (UrlElicitationRequired) and
    /// an [`ElicitationHandler`] is installed, the host is asked via the
    /// Claude control channel before the error is returned to the caller.
    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<String> {
        let params = serde_json::json!({
            "name": name,
            "arguments": arguments,
        });
        // Scope the switched window to exactly this request: the short
        // read timeout goes back in place even when the call fails, so a
        // later `initialize`-style request cannot inherit the long one.
        let previous = std::mem::replace(&mut self.read_timeout, self.tool_timeout);
        let outcome = self.send_request("tools/call", params).await;
        self.read_timeout = previous;

        let result: Value = match outcome {
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
        // Per-request budget (issue #181 review): `tools/call` gets the
        // configurable tool timeout, everything else the bounded default.
        // Applies to both HTTP transports; stdio tiers via `read_timeout`
        // / `tool_timeout` in `read_stdio_response` instead.
        let http_timeout = self.http_request_timeout(value);
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
                client,
                post_url,
                pending,
                waiters,
                ..
            } => {
                let body = serde_json::to_string(value)?;
                // Register the response slot *before* the POST: the reader
                // task can see the reply while `send()` is still in flight
                // (issue #178). Notifications (`no id`) expect no reply.
                let request_id = value.get("id").and_then(|v| v.as_u64());
                if let Some(id) = request_id {
                    let (tx, rx) = oneshot::channel();
                    pending.lock().await.insert(id, tx);
                    waiters.insert(id, rx);
                }

                let mut outcome = Ok(());
                match client
                    .post(post_url.as_str())
                    .header("Content-Type", "application/json")
                    .timeout(http_timeout)
                    .body(body)
                    .send()
                    .await
                {
                    Ok(response) if response.status().is_success() => {
                        // The reply travels on the SSE stream; drain this
                        // (usually empty `202`) body so the connection can
                        // be reused. A body-read error is not a request
                        // failure.
                        let _ = response.bytes().await;
                    }
                    Ok(response) => {
                        outcome = Err(Error::Mcp {
                            server: self.server_name.clone(),
                            message: format!(
                                "HTTP POST to `{post_url}` returned HTTP {}",
                                response.status()
                            ),
                        });
                    }
                    Err(e) => {
                        outcome = Err(Error::Mcp {
                            server: self.server_name.clone(),
                            message: format!("HTTP POST error: {e}"),
                        });
                    }
                }

                if outcome.is_err() {
                    // The request never reached the stream; drop its slot so
                    // no later read can wait on a reply that cannot come.
                    if let Some(id) = request_id {
                        pending.lock().await.remove(&id);
                        waiters.remove(&id);
                    }
                }
                outcome
            }
            McpTransport::StreamableHttp {
                client,
                url,
                session_id,
                pending,
            } => {
                let body = serde_json::to_string(value)?;
                let mut request = client
                    .post(url.as_str())
                    .header("Content-Type", "application/json")
                    .header("Accept", "application/json, text/event-stream")
                    .timeout(http_timeout)
                    .body(body);
                if let Some(sid) = session_id.as_deref() {
                    request = request.header("Mcp-Session-Id", sid);
                }
                let response = request.send().await.map_err(|e| Error::Mcp {
                    server: self.server_name.clone(),
                    message: format!("HTTP POST to `{url}` failed: {e}"),
                })?;

                // Capture / refresh the session id the server assigns
                // (usually on the initialize response).
                if let Some(sid) = response
                    .headers()
                    .get("mcp-session-id")
                    .and_then(|v| v.to_str().ok())
                {
                    *session_id = Some(sid.to_string());
                }

                if !response.status().is_success() {
                    return Err(Error::Mcp {
                        server: self.server_name.clone(),
                        message: format!(
                            "HTTP POST to `{url}` returned HTTP {}",
                            response.status()
                        ),
                    });
                }

                // Notifications get no JSON-RPC reply (202 Accepted, no body).
                let Some(expected_id) = value.get("id").and_then(|v| v.as_u64()) else {
                    // Drain the (typically empty) body so the connection can
                    // be reused instead of being torn down mid-stream.
                    let _ = response.bytes().await;
                    return Ok(());
                };

                let content_type = response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let text = response.text().await.map_err(|e| Error::Mcp {
                    server: self.server_name.clone(),
                    message: format!("failed to read HTTP response body from `{url}`: {e}"),
                })?;
                *pending = Some(parse_http_jsonrpc_response(
                    &text,
                    &content_type,
                    expected_id,
                    &self.server_name,
                )?);
                Ok(())
            }
        }
    }

    /// Read a JSON-RPC response matching the given id from the active transport.
    async fn read_response(&mut self, expected_id: u64) -> Result<Value> {
        // Copied out of `self` so the transport arm below can borrow the
        // transport mutably while these stay usable.
        let read_timeout = self.read_timeout;
        let server_name = self.server_name.clone();
        match &mut self.transport {
            McpTransport::Stdio { stdin, reader, .. } => {
                Self::read_stdio_response(stdin, reader, expected_id, &server_name, read_timeout)
                    .await
            }
            McpTransport::HttpSse {
                pending, waiters, ..
            } => {
                // The resident reader hands the reply over through the slot
                // `write_line` registered before the POST (issue #178).
                let Some(rx) = waiters.remove(&expected_id) else {
                    return Err(Error::Mcp {
                        server: server_name,
                        message: format!("no pending HTTP+SSE request for id {expected_id}"),
                    });
                };
                let pending = pending.clone();
                match timeout(read_timeout, rx).await {
                    Ok(Ok(result)) => result,
                    Ok(Err(_)) => Err(Error::Mcp {
                        server: server_name,
                        message: format!(
                            "SSE session ended before a response for id {expected_id} arrived"
                        ),
                    }),
                    Err(_) => {
                        // Stop the reader from dispatching into the dropped
                        // slot when the late reply finally shows up.
                        pending.lock().await.remove(&expected_id);
                        Err(Error::Mcp {
                            server: server_name,
                            message: format!(
                                "SSE timed out waiting for response for id {expected_id}"
                            ),
                        })
                    }
                }
            }
            McpTransport::StreamableHttp { pending, .. } => match pending.take() {
                Some(result) => Ok(result),
                None => Err(Error::Mcp {
                    server: server_name,
                    message: format!("no buffered HTTP response for id {expected_id}"),
                }),
            },
        }
    }

    /// Read a JSON-RPC response from stdio.
    ///
    /// Anything the server sends that is not the awaited response is
    /// handled rather than discarded (issue #181):
    ///
    /// * a **server-initiated request** (`id` + `method`) is answered on
    ///   the spot — `ping` with an empty result, anything else with
    ///   JSON-RPC `-32601` — so the peer never waits forever;
    /// * a **non-JSON line** (server logging to stdout) is logged at
    ///   `debug` and skipped, instead of aborting the pending call.
    async fn read_stdio_response(
        stdin: &mut ChildStdin,
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

                    let parsed: Value = match serde_json::from_str(trimmed) {
                        Ok(parsed) => parsed,
                        Err(e) => {
                            // Servers commonly print progress/log lines to
                            // stdout between protocol messages. Skipping is
                            // safe: a real response must be valid JSON, so
                            // it can never be one of these lines.
                            tracing::debug!(
                                target: "recursive::mcp",
                                server = %server_name,
                                error = %e,
                                line = %trimmed,
                                "skipping non-JSON line on MCP stdio stdout"
                            );
                            continue;
                        }
                    };

                    // A *request* from the server carries both `id` and
                    // `method`; a response carries `id` and `result`/`error`.
                    if let Some(req_id) = parsed.get("id") {
                        if let Some(method) = parsed.get("method").and_then(|v| v.as_str()) {
                            if let Err(e) =
                                Self::answer_server_request(stdin, req_id.clone(), method).await
                            {
                                tracing::debug!(
                                    target: "recursive::mcp",
                                    server = %server_name,
                                    error = %e,
                                    method = %method,
                                    "failed to answer server-initiated MCP request"
                                );
                            }
                            continue;
                        }
                    }

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

    /// Write a JSON-RPC reply to a server-initiated request (issue #181).
    ///
    /// `ping` is answered with an empty result; any other method — and any
    /// method we cannot honour — gets `-32601 Method not found`. Answering
    /// with an error rather than staying silent matters: the peer is
    /// waiting on this id and would otherwise block until its own
    /// timeout.
    async fn answer_server_request(
        stdin: &mut ChildStdin,
        request_id: Value,
        method: &str,
    ) -> std::io::Result<()> {
        // A null/missing id means a notification, not a request — it must
        // not be answered.
        if request_id.is_null() {
            return Ok(());
        }
        let reply = if method == "ping" {
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {},
            })
        } else {
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {
                    "code": -32601,
                    "message": format!("Method not found: {method}"),
                },
            })
        };
        let mut full = serde_json::to_vec(&reply)?;
        full.push(b'\n');
        stdin.write_all(&full).await?;
        stdin.flush().await
    }
}

// ---------------------------------------------------------------------------
// Legacy HTTP+SSE session reader (issue #178)
// ---------------------------------------------------------------------------

/// Resolve the `endpoint` event's payload to an absolute POST URL.
///
/// Official SDK servers announce a relative path (`/messages?sessionId=…`,
/// sometimes with the query only), which must be joined against the SSE
/// URL — POSTing the raw payload is an invalid-URL error.
fn resolve_sse_endpoint(base: &str, endpoint: &str, server: &str) -> Result<String> {
    let base_url = url::Url::parse(base).map_err(|e| Error::Mcp {
        server: server.to_string(),
        message: format!("invalid SSE endpoint URL `{base}`: {e}"),
    })?;
    base_url
        .join(endpoint.trim())
        .map(|url| url.to_string())
        .map_err(|e| Error::Mcp {
            server: server.to_string(),
            message: format!("SSE `endpoint` event returned an unusable URL `{endpoint}`: {e}"),
        })
}

/// Spawn the resident reader for a legacy SSE stream.
///
/// The task owns the session's `GET` response for the client's lifetime
/// (the client aborts it on `Drop`). It frames events incrementally, wakes
/// the waiter of every JSON-RPC response by id, and answers
/// server-initiated requests. On stream error or EOF it fails every
/// pending waiter, so a dropped connection surfaces as an error instead of
/// a hang.
fn spawn_sse_reader<S, B>(
    mut stream: S,
    initial: &[u8],
    pending: PendingResponses,
    post: reqwest::Client,
    post_url: String,
    server_name: String,
) -> JoinHandle<()>
where
    S: futures_util::Stream<Item = reqwest::Result<B>> + Unpin + Send + 'static,
    B: AsRef<[u8]> + Send + 'static,
{
    let mut parser = SseParser::new(initial);
    tokio::spawn(async move {
        loop {
            for event in parser.take() {
                dispatch_sse_event(event, &pending, &post, &post_url, &server_name).await;
            }
            match stream.next().await {
                Some(Ok(chunk)) => parser.push(chunk.as_ref()),
                Some(Err(e)) => {
                    fail_pending_sse(&pending, &server_name, format!("SSE stream error: {e}"))
                        .await;
                    return;
                }
                None => {
                    fail_pending_sse(
                        &pending,
                        &server_name,
                        "SSE stream closed by the server".to_string(),
                    )
                    .await;
                    return;
                }
            }
        }
    })
}

/// Route one SSE event: responses wake their waiter, requests are answered
/// on the POST endpoint, anything else is logged and skipped.
async fn dispatch_sse_event(
    event: SseEvent,
    pending: &PendingResponses,
    post: &reqwest::Client,
    post_url: &str,
    server_name: &str,
) {
    if event.data.is_empty() {
        return;
    }
    // Only meaningful during the handshake, which already consumed it.
    if event.event.as_deref() == Some("endpoint") {
        tracing::debug!(
            target: "recursive::mcp",
            server = %server_name,
            "ignoring a repeated `endpoint` event on an established MCP SSE stream"
        );
        return;
    }

    let Ok(message) = serde_json::from_str::<Value>(event.data.trim()) else {
        tracing::debug!(
            target: "recursive::mcp",
            server = %server_name,
            data = %event.data,
            "ignoring a non-JSON message on the MCP SSE stream"
        );
        return;
    };

    // A *request* from the server carries both `id` and `method`.
    if let Some(method) = message.get("method").and_then(|v| v.as_str()) {
        let request_id = message.get("id").cloned().unwrap_or(Value::Null);
        answer_sse_server_request(post, post_url, server_name, request_id, method).await;
        return;
    }

    let Some(id) = message.get("id").and_then(|v| v.as_u64()) else {
        tracing::debug!(
            target: "recursive::mcp",
            server = %server_name,
            "ignoring a server notification on the MCP SSE stream"
        );
        return;
    };
    let waiter = pending.lock().await.remove(&id);
    let Some(waiter) = waiter else {
        // Typically a reply that arrived after its caller timed out.
        tracing::debug!(
            target: "recursive::mcp",
            server = %server_name,
            id,
            "dropping an MCP SSE response with no pending waiter"
        );
        return;
    };
    let _ = waiter.send(jsonrpc_outcome(&message, server_name));
}

/// Answer a server-initiated JSON-RPC request over the message endpoint,
/// mirroring the stdio transport (issue #181): `ping` gets an empty
/// result, anything else `-32601`. Staying silent would block the peer
/// until its own timeout.
async fn answer_sse_server_request(
    post: &reqwest::Client,
    post_url: &str,
    server_name: &str,
    request_id: Value,
    method: &str,
) {
    // A null/missing id means a notification, not a request.
    if request_id.is_null() {
        return;
    }
    let reply = if method == "ping" {
        serde_json::json!({ "jsonrpc": "2.0", "id": request_id, "result": {} })
    } else {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "error": { "code": -32601, "message": format!("Method not found: {method}") },
        })
    };
    // The SSE client has no client-level timeout (the GET is long-lived),
    // so this POST must carry its own bounded budget.
    let outcome = post
        .post(post_url)
        .header("Content-Type", "application/json")
        .timeout(HTTP_DEFAULT_REQUEST_TIMEOUT)
        .body(reply.to_string())
        .send()
        .await;
    match outcome {
        Ok(response) => {
            let _ = response.bytes().await;
        }
        Err(e) => tracing::warn!(
            target: "recursive::mcp",
            server = %server_name,
            method = %method,
            error = %e,
            "failed to answer a server-initiated MCP request on the SSE transport"
        ),
    }
}

/// Fail every in-flight request after the stream is lost, so callers get a
/// prompt error rather than waiting out their own budget.
async fn fail_pending_sse(pending: &PendingResponses, server_name: &str, message: String) {
    let mut slots = pending.lock().await;
    if slots.is_empty() {
        return;
    }
    tracing::warn!(
        target: "recursive::mcp",
        server = %server_name,
        waiting = slots.len(),
        reason = %message,
        "MCP SSE stream lost; failing pending requests"
    );
    for (_, waiter) in slots.drain() {
        let _ = waiter.send(Err(Error::Mcp {
            server: server_name.to_string(),
            message: message.clone(),
        }));
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

/// One SSE event block, as framed by blank-line separators.
#[derive(Debug, Default, PartialEq, Eq)]
struct SseEvent {
    /// The `event:` field, when the server supplied one.
    event: Option<String>,
    /// Concatenated `data:` lines. Multi-line data is joined without a
    /// separator, matching [`parse_sse_response`] (a conforming server
    /// never splits one JSON value across data lines).
    data: String,
}

impl SseEvent {
    fn is_empty(&self) -> bool {
        self.event.is_none() && self.data.is_empty()
    }
}

/// Incremental SSE frame parser for the legacy transport's resident reader
/// (issue #178).
///
/// Bytes from a `bytes_stream` split anywhere — mid-field, mid-line, even
/// mid-character — so only complete `\n`-terminated lines are consumed and
/// the tail stays buffered. Working on bytes keeps a multi-byte UTF-8
/// sequence intact across chunks; each *complete* line is decoded
/// separately (a `\n` can never appear inside a multi-byte sequence).
struct SseParser {
    buf: Vec<u8>,
    /// Fields accumulated for the event currently being framed.
    current: SseEvent,
}

impl SseParser {
    fn new(initial: &[u8]) -> Self {
        Self {
            buf: initial.to_vec(),
            current: SseEvent::default(),
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// Drain every event completed by the buffered bytes.
    fn take(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        let mut consumed = 0usize;
        while let Some(offset) = self.buf[consumed..].iter().position(|b| *b == b'\n') {
            let end = consumed + offset;
            let raw = String::from_utf8_lossy(&self.buf[consumed..end]);
            let line = raw.trim_end_matches('\r');
            consumed = end + 1;

            if line.is_empty() {
                // Blank line terminates the event.
                if !self.current.is_empty() {
                    events.push(std::mem::take(&mut self.current));
                }
                continue;
            }
            let Some((field, value)) = line.split_once(':') else {
                // No colon → field with an empty value; nothing we read
                // (`event`, `data`) is meaningful that way.
                continue;
            };
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => self.current.event = Some(value.to_string()),
                "data" => self.current.data.push_str(value),
                // `id:` / `retry:` carry nothing we use: the stream is
                // bound to one session, so there is no resumption here.
                _ => {}
            }
        }
        self.buf.drain(..consumed);
        events
    }
}

/// Interpret one JSON-RPC message object, returning its `result` for the
/// expected id, or an error describing why it does not match / is invalid.
fn parse_jsonrpc_message(parsed: &Value, expected_id: u64, server_name: &str) -> Result<Value> {
    if parsed.get("id").and_then(|v| v.as_u64()) != Some(expected_id) {
        return Err(Error::Mcp {
            server: server_name.to_string(),
            message: format!("response does not match request id {expected_id}"),
        });
    }
    jsonrpc_outcome(parsed, server_name)
}

/// The `result` of a JSON-RPC response, or the server's error object as an
/// [`Error::Mcp`]. Shared by the per-request readers and the legacy SSE
/// reader, which does not know which request it is answering.
fn jsonrpc_outcome(parsed: &Value, server_name: &str) -> Result<Value> {
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
    Err(Error::Mcp {
        server: server_name.to_string(),
        message: "response missing both `result` and `error`".into(),
    })
}

/// Parse a Streamable HTTP POST response body. The body is either a JSON
/// JSON-RPC message (object or batch array) or an SSE-framed stream, per the
/// response `Content-Type`.
fn parse_http_jsonrpc_response(
    body: &str,
    content_type: &str,
    expected_id: u64,
    server_name: &str,
) -> Result<Value> {
    if content_type.contains("text/event-stream") {
        return parse_sse_response(body, expected_id, server_name).unwrap_or_else(|| {
            Err(Error::Mcp {
                server: server_name.to_string(),
                message: format!("SSE response contained no message for id {expected_id}"),
            })
        });
    }

    let parsed: Value = serde_json::from_str(body.trim()).map_err(|e| Error::Mcp {
        server: server_name.to_string(),
        message: format!("server returned non-JSON body: {e}"),
    })?;
    match parsed {
        Value::Array(items) => items
            .iter()
            .find(|item| item.get("id").and_then(|v| v.as_u64()) == Some(expected_id))
            .map(|item| parse_jsonrpc_message(item, expected_id, server_name))
            .unwrap_or_else(|| {
                Err(Error::Mcp {
                    server: server_name.to_string(),
                    message: format!("batch response contained no message for id {expected_id}"),
                })
            }),
        other => parse_jsonrpc_message(&other, expected_id, server_name),
    }
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
    /// Cap on the result bytes returned to the transcript. MCP servers can
    /// return unbounded payloads; without a cap a single call re-inflates
    /// every subsequent prompt. Defaults to
    /// [`DEFAULT_MCP_MAX_OUTPUT_BYTES`].
    max_output_bytes: usize,
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
            max_output_bytes: DEFAULT_MCP_MAX_OUTPUT_BYTES,
        }
    }

    /// Builder: set the trust level for this tool's server.
    pub fn with_trust(mut self, trust: McpServerTrust) -> Self {
        self.trust = trust;
        self
    }

    /// Builder: override the result-byte cap.
    pub fn with_max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = max_output_bytes;
        self
    }

    /// The server that owns this tool (used to preserve MCP tools across a
    /// container-tier registry rebuild — see `ToolRegistry::mcp_tools`).
    pub fn server_name(&self) -> &str {
        &self.server_name
    }
}

#[async_trait]
impl Tool for McpTool {
    fn is_deferred(&self) -> bool {
        true
    }

    fn mcp_server_name(&self) -> Option<&str> {
        Some(&self.server_name)
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
        let text = client.call_tool(&self.spec.name, arguments).await?;
        Ok(truncate_mcp_output(text, self.max_output_bytes))
    }
}

/// Cap an MCP tool result to `max_bytes`, appending a marker that names the
/// original size so the model knows the payload was cut. No-op when within
/// budget. Bytes are counted UTF-8-safely via [`crate::truncate_str`].
fn truncate_mcp_output(text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    let total = text.len();
    let truncated = crate::truncate_str(&text, max_bytes);
    format!("{truncated}\n\n[…truncated at {max_bytes} bytes; full result was {total} bytes]")
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
// Kill-gracefully: SIGTERM → grace period → SIGKILL (S2-E15)
// ---------------------------------------------------------------------------

/// Result of a graceful process kill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillOutcome {
    /// Process exited gracefully after SIGTERM.
    Graceful,
    /// Process did not exit in time; SIGKILL was sent.
    Timeout,
}

/// Kill a child process gracefully: send SIGTERM, wait for the grace
/// period, then send SIGKILL if still alive.
///
/// On non-Unix platforms (Windows), falls back to `child.kill()` immediately
/// since SIGTERM is not well-defined.
///
/// # Arguments
///
/// * `child` - The child process to kill.
/// * `grace_period` - How long to wait after SIGTERM before sending SIGKILL.
/// * `server_name` - Name of the server (for logging).
///
/// # Returns
///
/// The [`KillOutcome`] indicating whether the process exited gracefully
/// or had to be killed.
pub async fn kill_gracefully(
    child: &mut tokio::process::Child,
    grace_period: std::time::Duration,
    server_name: &str,
) -> KillOutcome {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        use tokio::time::timeout;

        // Phase 1: SIGTERM to the whole process group (Goal 412) — a launcher
        // (`npx`, `uvx`) leaves the real server as a child, and signalling only
        // the launcher would strand it.
        tracing::info!(server = %server_name, "kill-gracefully: sending SIGTERM");
        let group_pid = child.id();
        if let Some(pid) = group_pid {
            crate::process::signal_group(pid, crate::process::GroupSignal::Terminate);
        }
        let _ = child.start_kill(); // sends SIGTERM on Unix

        // Phase 2: wait for grace period
        match timeout(grace_period, child.wait()).await {
            Ok(Ok(status)) => {
                if let Some(signal) = status.signal() {
                    tracing::info!(
                        server = %server_name,
                        signal = signal,
                        "kill-gracefully: process exited via signal {signal}"
                    );
                } else {
                    tracing::info!(
                        server = %server_name,
                        "kill-gracefully: process exited gracefully"
                    );
                }
                // Anything the server spawned is still in its group; a
                // graceful exit of the leader is not proof the tree is gone.
                if let Some(pid) = group_pid {
                    crate::process::kill_group(pid);
                }
                KillOutcome::Graceful
            }
            Ok(Err(e)) => {
                // wait() error -- try SIGKILL as fallback
                tracing::warn!(
                    server = %server_name,
                    error = %e,
                    "kill-gracefully: wait() error after SIGTERM, sending SIGKILL"
                );
                crate::process::terminate_group(child).await;
                KillOutcome::Timeout
            }
            Err(_elapsed) => {
                // Phase 3: timeout -- SIGKILL the group, then reap the leader.
                tracing::warn!(
                    server = %server_name,
                    grace_ms = grace_period.as_millis(),
                    "kill-gracefully: SIGTERM timeout, sending SIGKILL"
                );
                crate::process::terminate_group(child).await;
                KillOutcome::Timeout
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = (grace_period, server_name);
        tracing::info!("kill-gracefully: non-Unix, sending kill immediately");
        let _ = child.start_kill();
        let _ = child.wait().await;
        KillOutcome::Graceful
    }
}

/// Default grace period for MCP subprocess shutdown (5 seconds).
/// Defined in `tools::run_background` so non-MCP callers (e.g. `acp`) can use
/// it without the `mcp` feature.
pub use crate::tools::run_background::DEFAULT_KILL_GRACE_PERIOD;

// ---------------------------------------------------------------------------
// MCP tool routing (S2-E17)
// ---------------------------------------------------------------------------

/// A mapping from tool name prefix to MCP server name.
///
/// When the agent issues a `tool_call`, the routing layer checks this
/// mapping to determine which MCP server should handle the call. Tools
/// that don't match any prefix are treated as native (non-MCP) tools
/// and are handled by the local ToolRegistry.
#[derive(Debug, Clone, Default)]
pub struct McpToolRouter {
    /// Maps server name → list of tool name prefixes registered by that server.
    /// Lookup is O(n * m) where n = servers, m = prefixes per server,
    /// but the number of servers is small (typically < 10).
    server_prefixes: std::collections::HashMap<String, Vec<String>>,
}

impl McpToolRouter {
    /// Create a new empty router.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register tools from a server. The `tool_names` are the raw tool names
    /// (without the `mcp__server__` prefix), and they are stored as-is for
    /// exact-match routing. Callers should also register prefixed names
    /// (e.g. `mcp__server__tool_name`) for MCP-prefixed dispatch.
    pub fn register_server(&mut self, server_name: &str, tool_names: &[String]) {
        let entry = self.server_prefixes.entry(server_name.to_string());
        let prefixes = entry.or_default();
        for name in tool_names {
            // Store both the raw name and the MCP-prefixed name.
            let prefixed = format!("mcp__{}__{}", server_name, name);
            if !prefixes.contains(name) {
                prefixes.push(name.clone());
            }
            if !prefixes.contains(&prefixed) {
                prefixes.push(prefixed);
            }
        }
    }

    /// Route a tool name to the server that handles it.
    ///
    /// Returns `Some(server_name)` if the tool matches any registered prefix.
    /// Returns `None` if no server handles this tool (it's a native tool).
    pub fn route(&self, tool_name: &str) -> Option<&str> {
        // Check MCP-prefixed names first: `mcp__<server>__<tool>`
        for (server, prefixes) in &self.server_prefixes {
            if prefixes.iter().any(|p| p == tool_name) {
                return Some(server.as_str());
            }
        }
        // Check raw names (exact match)
        for (server, prefixes) in &self.server_prefixes {
            if prefixes.iter().any(|p| p == tool_name) {
                return Some(server.as_str());
            }
        }
        None
    }
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

    // ── issue #181: stdio timeout / capability helpers ────────────────────

    /// One consolidated test for the process-global env override (the
    /// cargo test harness runs tests in parallel, so two tests touching
    /// the same env var would race — see `.dev/AGENTS.md`).
    #[test]
    fn default_tool_timeout_env_override_and_fallback() {
        std::env::remove_var(MCP_TOOL_TIMEOUT_ENV);
        assert_eq!(
            default_tool_timeout(),
            DEFAULT_MCP_TOOL_TIMEOUT,
            "no env var → the #181 default (300s) must apply"
        );

        std::env::set_var(MCP_TOOL_TIMEOUT_ENV, "42");
        assert_eq!(default_tool_timeout(), Duration::from_secs(42));

        // A usable value with surrounding whitespace still parses.
        std::env::set_var(MCP_TOOL_TIMEOUT_ENV, " 7 ");
        assert_eq!(default_tool_timeout(), Duration::from_secs(7));

        // Unusable values fall back to the default rather than to zero
        // (a zero timeout would fail every tool call instantly).
        for bad in ["", "abc", "0", "-1", "1.5"] {
            std::env::set_var(MCP_TOOL_TIMEOUT_ENV, bad);
            assert_eq!(
                default_tool_timeout(),
                DEFAULT_MCP_TOOL_TIMEOUT,
                "`{bad}` must fall back to the default"
            );
        }

        std::env::remove_var(MCP_TOOL_TIMEOUT_ENV);
    }

    #[test]
    fn with_tool_timeout_overrides_the_per_call_budget() {
        // No transport is exercised; we only assert the builder method's
        // effect on the fields.
        let client = idle_streamable_client(DEFAULT_STDIO_READ_TIMEOUT, DEFAULT_MCP_TOOL_TIMEOUT);
        assert_eq!(client.tool_timeout, DEFAULT_MCP_TOOL_TIMEOUT);
        assert_eq!(client.read_timeout, DEFAULT_STDIO_READ_TIMEOUT);

        let client = client.with_tool_timeout(Duration::from_secs(900));
        assert_eq!(client.tool_timeout, Duration::from_secs(900));
        assert_eq!(
            client.read_timeout, DEFAULT_STDIO_READ_TIMEOUT,
            "`with_tool_timeout` must not widen the handshake timeout"
        );
        // Review rework: the override must be visible on the HTTP tier too
        // (it used to be a silent no-op there).
        let call = serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call"});
        assert_eq!(
            client.http_request_timeout(&call),
            Duration::from_secs(900),
            "`with_tool_timeout` must reach the HTTP transports"
        );
    }

    #[test]
    fn has_capability_accepts_object_true_and_array() {
        // The spec wire shape.
        assert!(has_capability(Some(&serde_json::json!({}))));
        assert!(has_capability(Some(
            &serde_json::json!({"subscribe": true})
        )));
        // Legacy/loose shapes.
        assert!(has_capability(Some(&serde_json::json!(true))));
        assert!(has_capability(Some(&serde_json::json!([]))));
        // Not advertised.
        assert!(!has_capability(Some(&serde_json::json!(false))));
        assert!(!has_capability(Some(&serde_json::json!(null))));
        assert!(!has_capability(None));
    }

    /// A Streamable-HTTP client with no live transport, for asserting the
    /// pure per-request timeout policy.
    fn idle_streamable_client(read_timeout: Duration, tool_timeout: Duration) -> McpClient {
        McpClient {
            transport: McpTransport::StreamableHttp {
                client: reqwest::Client::new(),
                url: "http://example.invalid/mcp".into(),
                session_id: None,
                pending: None,
            },
            next_id: 1,
            capabilities: ServerCapabilities::default(),
            server_name: "probe".into(),
            read_timeout,
            tool_timeout,
            elicitation: None,
        }
    }

    #[test]
    fn http_request_timeout_is_tiered() {
        // Review rework of #181: a large tool budget must NOT leak into
        // the handshake — that is the "initialize must not take 5
        // minutes" trap.
        let client = idle_streamable_client(Duration::from_secs(10), Duration::from_secs(300));

        let call = serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call"});
        assert_eq!(client.http_request_timeout(&call), Duration::from_secs(300));
        // A configured tool budget below the read timeout is still honoured
        // for `tools/call` — the tiers are independent.
        let tight = idle_streamable_client(Duration::from_secs(10), Duration::from_secs(2));
        assert_eq!(tight.http_request_timeout(&call), Duration::from_secs(2));

        for method in ["initialize", "tools/list", "resources/list", "prompts/get"] {
            let request = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": method});
            assert_eq!(
                client.http_request_timeout(&request),
                Duration::from_secs(10),
                "`{method}` must follow the read timeout, never the tool budget"
            );
        }

        // A short read budget is honoured as-is: this is what makes the
        // "server never started" failure fast even when the tool budget is
        // minutes long.
        let impatient =
            idle_streamable_client(Duration::from_millis(300), Duration::from_secs(300));
        let init = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"});
        assert_eq!(
            impatient.http_request_timeout(&init),
            Duration::from_millis(300)
        );

        // A notification matches no method tier either → read budget.
        let notification =
            serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert_eq!(
            client.http_request_timeout(&notification),
            Duration::from_secs(10)
        );
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

    // ── issue #104: remote auth headers + transport selection ─────────────

    fn server_with_url(url: &str, transport: Option<&str>) -> McpServer {
        McpServer {
            name: "remote".into(),
            command: String::new(),
            args: vec![],
            url: Some(url.into()),
            env: None,
            headers: None,
            transport: transport.map(str::to_string),
        }
    }

    #[test]
    fn resolve_remote_transport_defaults_to_auto() {
        assert_eq!(resolve_remote_transport(None, "s"), RemoteTransport::Auto);
        assert_eq!(
            resolve_remote_transport(Some(""), "s"),
            RemoteTransport::Auto
        );
        assert_eq!(
            resolve_remote_transport(Some("auto"), "s"),
            RemoteTransport::Auto
        );
    }

    #[test]
    fn resolve_remote_transport_maps_explicit_values() {
        assert_eq!(
            resolve_remote_transport(Some("sse"), "s"),
            RemoteTransport::LegacySse
        );
        assert_eq!(
            resolve_remote_transport(Some("http"), "s"),
            RemoteTransport::StreamableHttp
        );
        assert_eq!(
            resolve_remote_transport(Some("streamable-http"), "s"),
            RemoteTransport::StreamableHttp
        );
    }

    #[test]
    fn resolve_remote_transport_unknown_falls_back_to_auto() {
        assert_eq!(
            resolve_remote_transport(Some("carrier-pigeon"), "s"),
            RemoteTransport::Auto
        );
    }

    #[test]
    fn expand_env_substitutes_known_and_empties_unknown() {
        std::env::set_var("RECURSIVE_MCP_TEST_TOK", "s3cret");
        assert_eq!(
            expand_env("Bearer ${RECURSIVE_MCP_TEST_TOK}"),
            "Bearer s3cret"
        );
        // Unknown var → empty string.
        assert_eq!(expand_env("x${RECURSIVE_MCP_TEST_DEFINITELY_UNSET}y"), "xy");
        // No placeholders → unchanged.
        assert_eq!(expand_env("plain"), "plain");
        // Unterminated placeholder is left literal.
        assert_eq!(expand_env("a${OPS"), "a${OPS");
    }

    #[tokio::test]
    async fn discover_parses_headers_and_transport_fields() {
        let dir = tempfile::TempDir::new().unwrap();
        tokio::fs::write(
            dir.path().join(".mcp.json"),
            r#"{
                "mcpServers": {
                    "github": {
                        "url": "https://api.example.com/mcp",
                        "transport": "http",
                        "headers": { "Authorization": "Bearer ${GITHUB_TOKEN}" }
                    }
                }
            }"#,
        )
        .await
        .unwrap();

        let servers = discover_mcp_servers(dir.path()).await.unwrap();
        assert_eq!(servers.len(), 1);
        let gh = &servers[0];
        assert_eq!(gh.transport.as_deref(), Some("http"));
        let headers = gh.headers.as_ref().expect("headers must be parsed");
        assert_eq!(
            headers.get("Authorization").map(String::as_str),
            Some("Bearer ${GITHUB_TOKEN}")
        );
    }

    // ── issue #104: result truncation ────────────────────────────────────

    #[test]
    fn truncate_mcp_output_within_budget_is_untouched() {
        let s = "hello world".to_string();
        assert_eq!(truncate_mcp_output(s.clone(), 100), s);
    }

    #[test]
    fn truncate_mcp_output_at_exact_budget_is_untouched() {
        let s = "abcde".to_string();
        assert_eq!(truncate_mcp_output(s.clone(), 5), s);
    }

    #[test]
    fn truncate_mcp_output_over_budget_appends_marker() {
        let s = "x".repeat(100);
        let out = truncate_mcp_output(s.clone(), 10);
        assert!(out.starts_with(&"x".repeat(10)), "prefix kept: {out}");
        assert!(out.contains("truncated at 10 bytes"), "marker: {out}");
        assert!(out.contains("full result was 100 bytes"), "size: {out}");
        assert!(out.len() > 10, "marker must be appended after the prefix");
    }

    #[test]
    fn truncate_mcp_output_respects_multibyte_boundary() {
        // 4 chars × 3 bytes = 12 bytes. max=4 lands mid-character.
        let s = "你好世界".to_string();
        let out = truncate_mcp_output(s, 4);
        // Must not panic and must remain valid UTF-8 (truncate_str guarantees this).
        assert!(out.is_char_boundary(out.find('\n').unwrap_or(out.len())));
        assert!(out.contains("truncated at 4 bytes"));
    }

    // ── issue #104: Streamable HTTP response parsing ─────────────────────

    #[test]
    fn parse_http_jsonrpc_response_plain_json_object() {
        let body = r#"{"jsonrpc":"2.0","id":7,"result":{"ok":true}}"#;
        let v = parse_http_jsonrpc_response(body, "application/json", 7, "s").unwrap();
        assert_eq!(v["ok"], serde_json::json!(true));
    }

    #[test]
    fn parse_http_jsonrpc_response_batch_array_picks_matching_id() {
        let body =
            r#"[{"jsonrpc":"2.0","id":1,"result":"a"},{"jsonrpc":"2.0","id":2,"result":"b"}]"#;
        let v = parse_http_jsonrpc_response(body, "application/json", 2, "s").unwrap();
        assert_eq!(v, serde_json::json!("b"));
    }

    #[test]
    fn parse_http_jsonrpc_response_batch_missing_id_errors() {
        let body = r#"[{"jsonrpc":"2.0","id":1,"result":"a"}]"#;
        let err = parse_http_jsonrpc_response(body, "application/json", 99, "s").unwrap_err();
        assert!(err.to_string().contains("99"), "{err}");
    }

    #[test]
    fn parse_http_jsonrpc_response_sse_framed_body() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"n\":1}}\n\n";
        let v =
            parse_http_jsonrpc_response(body, "text/event-stream; charset=utf-8", 3, "s").unwrap();
        assert_eq!(v["n"], serde_json::json!(1));
    }

    #[test]
    fn parse_http_jsonrpc_response_propagates_jsonrpc_error() {
        let body = r#"{"jsonrpc":"2.0","id":5,"error":{"code":-32601,"message":"nope"}}"#;
        let err = parse_http_jsonrpc_response(body, "application/json", 5, "s").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("-32601") && msg.contains("nope"), "{msg}");
    }

    // ── issue #104: Streamable HTTP end-to-end against a loopback mock ────

    /// A blocking loopback HTTP server that speaks enough Streamable HTTP
    /// for one client session. Records every raw request head for header
    /// assertions. One connection per request (`Connection: close`).
    fn spawn_streamable_mock(captured: Arc<std::sync::Mutex<Vec<String>>>) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buf = vec![0u8; 16384];
                let n = stream.read(&mut buf).unwrap_or(0);
                let raw = String::from_utf8_lossy(&buf[..n]).into_owned();
                let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
                captured.lock().unwrap().push(head.to_string());
                let request: Value = serde_json::from_str(body).unwrap_or(Value::Null);
                let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
                let id = request.get("id").cloned();
                let (status, extra, payload) = match method {
                    "initialize" => (
                        "200 OK",
                        "Mcp-Session-Id: s-1\r\n",
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "protocolVersion": "2025-03-26",
                                "capabilities": {},
                                "serverInfo": {"name": "mock", "version": "1"}
                            }
                        })
                        .to_string(),
                    ),
                    "notifications/initialized" => ("202 Accepted", "", String::new()),
                    "tools/list" => (
                        "200 OK",
                        "",
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]}
                        })
                        .to_string(),
                    ),
                    "tools/call" => (
                        "200 OK",
                        "",
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {"content": [{"type": "text", "text": "pong"}]}
                        })
                        .to_string(),
                    ),
                    _ => (
                        "200 OK",
                        "",
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {"code": -32601, "message": "unknown"}
                        })
                        .to_string(),
                    ),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{addr}/mcp")
    }

    #[tokio::test]
    async fn streamable_http_initialize_and_list_tools() {
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let url = spawn_streamable_mock(captured.clone());
        // transport=None → auto → Streamable HTTP succeeds, no SSE fallback.
        let server = server_with_url(&url, None);
        let mut client = McpClient::spawn(&server).await.expect("spawn");
        let tools = client.list_tools().await.expect("list_tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        let out = client
            .call_tool("echo", serde_json::json!({}))
            .await
            .expect("call");
        assert_eq!(out, "pong");

        let heads = captured.lock().unwrap().clone();
        let init_head = heads
            .iter()
            .find(|h| h.starts_with("POST /mcp"))
            .expect("a POST must have been recorded");
        assert!(
            init_head
                .to_lowercase()
                .contains("accept: application/json, text/event-stream"),
            "streamable POST must advertise both response types: {init_head}"
        );
    }

    #[tokio::test]
    async fn streamable_http_sends_configured_auth_header() {
        std::env::set_var("RECURSIVE_MCP_TEST_AUTH", "tok-123");
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let url = spawn_streamable_mock(captured.clone());
        let mut server = server_with_url(&url, Some("http"));
        server.headers = Some(HashMap::from([(
            "Authorization".to_string(),
            "Bearer ${RECURSIVE_MCP_TEST_AUTH}".to_string(),
        )]));
        let _client = McpClient::spawn(&server).await.expect("spawn");
        let heads = captured.lock().unwrap().clone();
        let head = heads.first().expect("at least one request");
        assert!(
            head.to_lowercase()
                .contains("authorization: bearer tok-123"),
            "auth header (with env expansion) must be sent: {head}"
        );
    }

    // ── issue #181 review: HTTP per-request budgets (hermetic) ───────────

    /// Spawn a hermetic loopback Streamable-HTTP mock. `initialize_delay`
    /// applies to the `initialize` response and `tools_call_delay` to the
    /// `tools/call` response — matching the client's two timeout tiers.
    /// Every other method (including the `notifications/initialized` POST,
    /// which reqwest still waits on) answers at once, so each test measures
    /// the tier under test and nothing else. One connection per request,
    /// `Connection: close`. No network access beyond localhost.
    fn spawn_delayed_streamable_mock(
        initialize_delay: Duration,
        tools_call_delay: Duration,
    ) -> String {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                // Read the head, then exactly `Content-Length` bytes of
                // body. Do NOT read to EOF: reqwest keeps its request side
                // open while it awaits the response, so `read_to_end` would
                // deadlock until the client's own timeout fires.
                let Ok(clone) = stream.try_clone() else { break };
                let mut reader = BufReader::new(clone);
                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) => {
                            if line.trim().is_empty() {
                                break;
                            }
                            let lower = line.to_ascii_lowercase();
                            if let Some(value) = lower.strip_prefix("content-length:") {
                                content_length = value.trim().parse::<usize>().unwrap_or(0);
                            }
                        }
                        Err(_) => break,
                    }
                }
                let mut buf = vec![0u8; content_length];
                if reader.read_exact(&mut buf).is_err() {
                    continue;
                }
                let body = String::from_utf8_lossy(&buf).into_owned();
                let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
                let id = request.get("id").cloned().unwrap_or(Value::Null);
                let delay = match method {
                    "initialize" => initialize_delay,
                    "tools/call" => tools_call_delay,
                    _ => Duration::ZERO,
                };
                std::thread::sleep(delay);
                let payload = match method {
                    "initialize" => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": "2025-03-26",
                            "capabilities": {},
                            "serverInfo": {"name": "slow-mock", "version": "1"}
                        }
                    }),
                    "tools/list" => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "tools": [{
                                "name": "slow",
                                "description": "Slow tool",
                                "inputSchema": {"type": "object"}
                            }]
                        }
                    }),
                    "tools/call" => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"content": [{"type": "text", "text": "slow: done"}]}
                    }),
                    _ => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {}
                    }),
                }
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                // The client may already have timed out and closed the
                // socket — that is the expected case in the fast-failure
                // test, so a write error is not a failure.
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{addr}/mcp")
    }

    /// Review rework: the HTTP `initialize` handshake must stay bounded by
    /// the read timeout. Its budget must NOT follow the (much larger)
    /// configurable tool timeout — that is exactly how "the server never
    /// started" would take 5 minutes to surface.
    ///
    /// Hermetic: the mock is a loopback listener that answers after 3s.
    /// The delay is far beyond both the 300ms read budget and the 2s
    /// latency bound below, and the tool budget (10s) is far beyond the
    /// delay, so a pass can only mean the read-budget tier held — any
    /// regression to the tool budget or the client backstop returns `Ok`
    /// at 3s and trips the first assert.
    #[tokio::test]
    async fn http_initialize_fails_fast_and_ignores_the_tool_budget() {
        let url = spawn_delayed_streamable_mock(Duration::from_secs(3), Duration::ZERO);
        let server = server_with_url(&url, Some("http"));

        let started = std::time::Instant::now();
        let result = McpClient::spawn_with_timeout(&server, Duration::from_millis(300))
            .await
            .map(|client| client.with_tool_timeout(Duration::from_secs(10)));
        let elapsed = started.elapsed();

        assert!(
            result.is_err(),
            "a 3s initialize must fail under a 300ms budget, not wait for the tool budget"
        );
        // The bound sits well above the 300ms budget so scheduler jitter
        // under a fully loaded parallel test run cannot flake it (observed
        // ~300-310ms idle, ~900ms under load) yet stays below the 3s mock
        // delay and the 10s tool budget: an error this fast is only
        // reachable via the read-budget tier.
        eprintln!("http initialize failed after {elapsed:?}");
        assert!(
            elapsed < Duration::from_secs(2),
            "initialize must fail promptly (read-timeout bound), took {elapsed:?} — \
             the per-request budget is not tiered"
        );
    }

    /// Review rework: `tools/call` is the one request that gets the
    /// configurable budget, so a call slower than the read timeout but
    /// within `tool_timeout` must succeed — over HTTP too, not just stdio.
    #[tokio::test]
    async fn http_tools_call_uses_the_configured_tool_budget() {
        // initialize answers immediately; tools/* answers after 800ms, so
        // the 800ms call exceeds a 300ms read budget but fits a 5s tool
        // budget, and the 30s floor cannot mask the difference.
        let url = spawn_delayed_streamable_mock(Duration::ZERO, Duration::from_millis(800));
        let server = server_with_url(&url, Some("http"));

        let mut client = McpClient::spawn_with_timeout(&server, Duration::from_millis(300))
            .await
            .expect("initialize answers immediately")
            .with_tool_timeout(Duration::from_secs(5));

        let started = std::time::Instant::now();
        let out = client
            .call_tool("slow", serde_json::json!({}))
            .await
            .expect("a 800ms tools/call must survive a 5s tool budget");
        assert_eq!(out, "slow: done");
        eprintln!("http tools/call succeeded after {:?}", started.elapsed());
        assert!(
            started.elapsed() >= Duration::from_millis(800),
            "the mock must actually have delayed the call (elapsed {:?})",
            started.elapsed()
        );
    }

    /// Review rework, the other half of the tier: the per-request budget
    /// must be *binding* on `tools/call`, not merely absent. The mock
    /// stalls 2.5s while the tool budget is 700ms, so this can only fail
    /// if reqwest actually applied the per-request timeout — falling back
    /// to the 30s client-level backstop would let the call succeed (that
    /// is exactly what negative control nc5 showed).
    #[tokio::test]
    async fn http_tools_call_is_cut_off_by_a_short_configured_budget() {
        let url = spawn_delayed_streamable_mock(Duration::ZERO, Duration::from_millis(2500));
        let server = server_with_url(&url, Some("http"));

        let mut client = McpClient::spawn_with_timeout(&server, Duration::from_secs(5))
            .await
            .expect("initialize answers immediately")
            .with_tool_timeout(Duration::from_millis(700));

        let started = std::time::Instant::now();
        let result = client.call_tool("slow", serde_json::json!({})).await;
        let elapsed = started.elapsed();
        eprintln!("http tools/call cut off after {elapsed:?}");
        assert!(
            result.is_err(),
            "a 2.5s tools/call must be cut off by a 700ms tool budget"
        );
        assert!(
            elapsed < Duration::from_millis(2000),
            "the configured tool budget must bind (expected ~700ms), took {elapsed:?} — \
             the call fell back to the fixed client-level backstop"
        );
    }

    // ── issue #178: legacy HTTP+SSE handshake and session reuse ──────────

    /// State shared with the loopback legacy-SSE mock.
    #[derive(Default)]
    struct LegacySseState {
        /// How many `GET`s opened the long-lived event stream.
        get_count: std::sync::atomic::AtomicUsize,
        /// Every POST target (path + query).
        post_targets: std::sync::Mutex<Vec<String>>,
        /// Every session id the server minted or was handed back on a POST.
        session_ids: std::sync::Mutex<Vec<String>>,
        /// Raw JSON bodies of every POST, for assertions.
        post_bodies: std::sync::Mutex<Vec<String>>,
        /// Queue feeding the live stream (installed by the GET handler).
        stream: std::sync::Mutex<Option<std::sync::mpsc::Sender<String>>>,
        /// Close the stream instead of answering this JSON-RPC method.
        close_stream_on: std::sync::Mutex<Option<String>>,
    }

    /// A loopback mock of the 2024-11-05 HTTP+SSE transport: one long-lived
    /// `GET` stream announcing a **relative** `endpoint`, and a `POST`
    /// endpoint that queues JSON-RPC replies onto that stream. Every
    /// connection gets its own thread, so the stream blocks without
    /// stalling POSTs.
    fn spawn_legacy_sse_mock(state: Arc<LegacySseState>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let state = state.clone();
                std::thread::spawn(move || handle_legacy_sse_conn(stream, state));
            }
        });
        format!("http://{addr}/sse")
    }

    fn handle_legacy_sse_conn(mut stream: std::net::TcpStream, state: Arc<LegacySseState>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let Ok(clone) = stream.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(clone);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() {
            return;
        }
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => return,
            }
            if line.trim().is_empty() {
                break;
            }
            let lower = line.to_ascii_lowercase();
            if let Some(value) = lower.strip_prefix("content-length:") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; content_length];
        if content_length > 0 && reader.read_exact(&mut body).is_err() {
            return;
        }
        let body = String::from_utf8_lossy(&body).into_owned();

        if request_line.starts_with("GET ") {
            // The session's single long-lived event stream.
            let n = state
                .get_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            let session = format!("session-{n}");
            state.session_ids.lock().unwrap().push(session.clone());
            let (tx, rx) = std::sync::mpsc::channel::<String>();
            *state.stream.lock().unwrap() = Some(tx);
            let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                        Cache-Control: no-cache\r\n\r\n";
            if stream.write_all(head.as_bytes()).is_err() {
                return;
            }
            // Relative on purpose: the client must join this against the
            // SSE URL instead of POSTing it verbatim (issue #178).
            let endpoint = format!("event: endpoint\ndata: /messages?sessionId={session}\n\n");
            if stream.write_all(endpoint.as_bytes()).is_err() {
                return;
            }
            let _ = stream.flush();
            while let Ok(payload) = rx.recv() {
                if write!(stream, "event: message\ndata: {payload}\n\n").is_err() {
                    break;
                }
                let _ = stream.flush();
            }
            return;
        }

        // POST /messages?sessionId=…
        let target = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or("")
            .to_string();
        state.post_targets.lock().unwrap().push(target.clone());
        if let Some(session) = target.split("sessionId=").nth(1) {
            state
                .session_ids
                .lock()
                .unwrap()
                .push(session.split('&').next().unwrap_or("").to_string());
        }
        state.post_bodies.lock().unwrap().push(body.clone());

        let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let close_on = state.close_stream_on.lock().unwrap().clone();
        if close_on.as_deref() == Some(method) {
            // Simulate a dropped stream: drop the queue sender (the GET
            // thread then returns and closes the socket) and answer the
            // POST so the client is left waiting on the stream alone.
            *state.stream.lock().unwrap() = None;
            let _ = stream.write_all(
                b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            let _ = stream.flush();
            return;
        }

        if !method.is_empty() {
            let id = request.get("id").cloned().unwrap_or(Value::Null);
            if !id.is_null() {
                let payload = match method {
                    "initialize" => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": "2024-11-05",
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "legacy-mock", "version": "1"}
                        }
                    }),
                    "tools/list" => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"tools": [{
                            "name": "echo",
                            "description": "Echo",
                            "inputSchema": {"type": "object"}
                        }]}
                    }),
                    "tools/call" => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"content": [{"type": "text", "text": "pong"}]}
                    }),
                    _ => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32601, "message": "unknown"}
                    }),
                };
                let queued = state
                    .stream
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|tx| tx.send(payload.to_string()).is_ok());
                assert_eq!(queued, Some(true), "the live stream must accept replies");
            }
            // Server-initiated request: the client answers it over POST.
            if method == "tools/list" {
                let ping =
                    serde_json::json!({"jsonrpc": "2.0", "id": "srv-ping-1", "method": "ping"});
                let _ = state
                    .stream
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|tx| tx.send(ping.to_string()));
            }
        }
        let _ = stream
            .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let _ = stream.flush();
    }

    #[test]
    fn sse_parser_frames_events_split_across_chunks() {
        let mut parser = SseParser::new(b"event: endpoint\ndata: /messages?sessionId=a\n\n");
        let events = parser.take();
        assert_eq!(
            events.len(),
            1,
            "the handshake buffer must frame: {events:?}"
        );
        assert_eq!(events[0].event.as_deref(), Some("endpoint"));
        assert_eq!(events[0].data, "/messages?sessionId=a");

        // A chunk boundary in the middle of a field, a line, and an event.
        parser.push(b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1");
        assert!(
            parser.take().is_empty(),
            "a partial event must not be dispatched"
        );
        parser.push(",\"result\":{\"text\":\"你好\"}}\n\n".as_bytes());
        let events = parser.take();
        assert_eq!(
            events[0].data, "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"text\":\"你好\"}}",
            "data must survive chunking byte-for-byte (UTF-8 safe)"
        );

        // CRLF framing and a comment line are handled.
        parser.push(b": keep-alive\r\nevent: message\r\ndata: {}\r\n\r\n");
        let events = parser.take();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].data, "{}");
    }

    #[test]
    fn resolve_sse_endpoint_joins_relative_against_the_stream_url() {
        assert_eq!(
            resolve_sse_endpoint("http://h:1/sse", "/messages?sessionId=x", "s").unwrap(),
            "http://h:1/messages?sessionId=x",
            "a root-relative endpoint must resolve against the SSE origin"
        );
        assert_eq!(
            resolve_sse_endpoint("http://h:1/sse", "messages?sessionId=x", "s").unwrap(),
            "http://h:1/messages?sessionId=x"
        );
        assert_eq!(
            resolve_sse_endpoint("http://h:1/sse", "http://other:2/m", "s").unwrap(),
            "http://other:2/m",
            "an absolute endpoint is used as-is"
        );

        let err = resolve_sse_endpoint("http://h:1/sse", "http://[::1", "s").unwrap_err();
        assert!(err.to_string().contains("unusable URL"), "{err}");
    }

    #[tokio::test]
    async fn legacy_sse_completes_the_handshake_and_reuses_one_session() {
        let state = Arc::new(LegacySseState::default());
        let url = spawn_legacy_sse_mock(state.clone());
        let server = server_with_url(&url, Some("sse"));

        // The handshake itself is the first regression: the old client
        // either failed on the relative endpoint URL or timed out.
        let mut client = McpClient::spawn(&server)
            .await
            .expect("legacy SSE initialize must complete");

        let tools = client.list_tools().await.expect("tools/list over SSE");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        let out = client
            .call_tool("echo", serde_json::json!({}))
            .await
            .expect("tools/call over SSE");
        assert_eq!(out, "pong");

        // One stream for the whole session. The old client re-`GET` the SSE
        // URL for every read, which opened a fresh server session each time.
        let gets = state.get_count.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(gets, 1, "the session must reuse its single SSE stream");

        let ids = state.session_ids.lock().unwrap().clone();
        assert!(ids.len() >= 3, "expected GET + several POSTs, got {ids:?}");
        assert!(
            ids.iter().all(|id| id == &ids[0]),
            "every request must stay on one sessionId, got {ids:?}"
        );

        let targets = state.post_targets.lock().unwrap().clone();
        assert!(
            !targets.is_empty()
                && targets
                    .iter()
                    .all(|t| t == &format!("/messages?sessionId={}", ids[0])),
            "the relative endpoint must be joined against the SSE URL: {targets:?}"
        );

        // The server-initiated `ping` shares the stream; the client must
        // answer it over POST rather than leave the server waiting.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let bodies = state.post_bodies.lock().unwrap().clone();
            if bodies.iter().any(|b| b.contains("\"srv-ping-1\"")) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the server ping must be answered over POST; saw {bodies:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn legacy_sse_stream_loss_fails_the_pending_request() {
        let state = Arc::new(LegacySseState::default());
        *state.close_stream_on.lock().unwrap() = Some("tools/call".into());
        let url = spawn_legacy_sse_mock(state.clone());
        let server = server_with_url(&url, Some("sse"));

        // A 30s budget: the error below must come from the dropped stream,
        // not from the caller's timeout.
        let mut client = McpClient::spawn_with_timeout(&server, Duration::from_secs(30))
            .await
            .expect("initialize answers before the stream drops");

        let started = std::time::Instant::now();
        let err = client
            .call_tool("echo", serde_json::json!({}))
            .await
            .expect_err("a dropped stream must surface as an error, not hang");
        let elapsed = started.elapsed();
        assert!(
            err.to_string().contains("SSE stream closed"),
            "the loss itself must be reported: {err}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the pending request must fail on stream loss, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn legacy_sse_rejects_a_non_stream_response() {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let Ok(clone) = stream.try_clone() else {
                    return;
                };
                let mut reader = BufReader::new(clone);
                let mut line = String::new();
                while let Ok(n) = reader.read_line(&mut line) {
                    if n == 0 || line.trim().is_empty() {
                        break;
                    }
                    line.clear();
                }
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                      Content-Length: 2\r\nConnection: close\r\n\r\n{}",
                );
                let _ = stream.flush();
            }
        });

        let server = server_with_url(&format!("http://{addr}/sse"), Some("sse"));
        let err = McpClient::spawn(&server)
            .await
            .expect_err("a JSON body is not an SSE stream");
        assert!(
            err.to_string().contains("text/event-stream"),
            "the Content-Type must be validated: {err}"
        );
    }
}
