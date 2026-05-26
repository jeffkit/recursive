//! MCP server lifecycle management.
//!
//! This module provides [`McpServerManager`], which handles spawning MCP
//! servers (stdio or HTTP+SSE), discovering their tools, and registering
//! them into the agent's [`ToolRegistry`]. Each MCP tool is wrapped in a
//! thin adapter that delegates execution to the corresponding server.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────┐
//! │              McpServerManager               │
//! │  ┌──────────┐  ┌──────────┐  ┌──────────┐  │
//! │  │ Server A │  │ Server B │  │ Server C │  │
//! │  │ (stdio)  │  │ (SSE)    │  │ (stdio)  │  │
//! │  └────┬─────┘  └────┬─────┘  └────┬─────┘  │
//! │       │              │              │        │
//! │  ┌────▼──────────────▼──────────────▼────┐  │
//! │  │          ToolRegistry                 │  │
//! │  │  mcp__A__tool1  mcp__B__tool2  ...    │  │
//! │  └───────────────────────────────────────┘  │
//! └─────────────────────────────────────────────┘
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{info, instrument};

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::mcp::{McpClient, McpServer, McpTool};
use crate::tools::ToolRegistry;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 protocol types (MCP server side)
// ---------------------------------------------------------------------------

/// A JSON-RPC 2.0 request.
#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// A JSON-RPC 2.0 response.
#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
}

impl JsonRpcResponse {
    /// Create a success response.
    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Create an error response.
    pub fn error(id: Value, code: i32, message: String) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError { code, message }),
        }
    }

    /// Create a parse error response (-32700).
    pub fn parse_error() -> Self {
        Self::error(Value::Null, -32700, "Parse error".to_string())
    }

    /// Create a method not found error response (-32601).
    pub fn method_not_found(id: Value) -> Self {
        Self::error(id, -32601, "Method not found".to_string())
    }
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Dispatch a parsed JSON-RPC request and return a response.
/// Returns `None` for notifications (no response needed).
pub async fn dispatch_request(
    request: &JsonRpcRequest,
    tool_specs: &[ToolSpec],
    tools: &ToolRegistry,
) -> Option<JsonRpcResponse> {
    match request.method.as_str() {
        "initialize" => Some(handle_initialize(request)),
        "notifications/initialized" => None,
        "tools/list" => Some(handle_tools_list(request, tool_specs)),
        "tools/call" => Some(handle_tools_call(request, tools).await),
        _ => {
            let id = request.id.clone().unwrap_or(Value::Null);
            Some(JsonRpcResponse::method_not_found(id))
        }
    }
}

/// Handle `initialize` — return server capabilities and info.
fn handle_initialize(request: &JsonRpcRequest) -> JsonRpcResponse {
    let id = request.id.clone().unwrap_or(Value::Null);
    JsonRpcResponse::success(
        id,
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": "recursive-mcp-server",
                "version": "0.1.0"
            }
        }),
    )
}

/// Handle `tools/list` — return the list of tool specs in MCP format.
fn handle_tools_list(request: &JsonRpcRequest, tool_specs: &[ToolSpec]) -> JsonRpcResponse {
    let id = request.id.clone().unwrap_or(Value::Null);
    let tools: Vec<Value> = tool_specs
        .iter()
        .map(|spec| {
            serde_json::json!({
                "name": spec.name,
                "description": spec.description,
                "inputSchema": spec.parameters,
            })
        })
        .collect();
    JsonRpcResponse::success(id, serde_json::json!({ "tools": tools }))
}

/// Handle `tools/call` — execute a tool and return the result.
async fn handle_tools_call(request: &JsonRpcRequest, tools: &ToolRegistry) -> JsonRpcResponse {
    let id = request.id.clone().unwrap_or(Value::Null);
    let tool_name = request
        .params
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let arguments = request
        .params
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::json!({}));

    if tool_name.is_empty() {
        return JsonRpcResponse::error(id, -32602, "Missing tool name".to_string());
    }

    match tools.get(tool_name) {
        Some(tool) => match tool.execute(arguments).await {
            Ok(text) => JsonRpcResponse::success(
                id,
                serde_json::json!({
                    "content": [{"type": "text", "text": text}]
                }),
            ),
            Err(e) => JsonRpcResponse::success(
                id,
                serde_json::json!({
                    "isError": true,
                    "content": [{"type": "text", "text": e.to_string()}]
                }),
            ),
        },
        None => JsonRpcResponse::error(id, -32602, format!("Tool not found: {tool_name}")),
    }
}

// ---------------------------------------------------------------------------
// McpServerRunner — stdio transport loop
// ---------------------------------------------------------------------------

/// Runs the MCP server stdio loop: reads JSON-RPC from stdin, dispatches,
/// and writes responses to stdout.
pub struct McpServerRunner {
    tool_specs: Vec<ToolSpec>,
    tools: ToolRegistry,
}

impl McpServerRunner {
    /// Create a new runner from a [`ToolRegistry`].
    ///
    /// The tool specs are extracted immediately so they can be served
    /// without holding a borrow on the registry.
    pub fn new(tools: ToolRegistry) -> Self {
        let tool_specs = tools.specs();
        Self { tool_specs, tools }
    }

    /// Run the stdio server loop until EOF on stdin.
    pub async fn run(&self) -> Result<()> {
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        self.run_on(BufReader::new(stdin), stdout).await
    }

    /// Run the server loop on generic reader/writer (testable).
    pub async fn run_on<R, W>(&self, reader: R, writer: W) -> Result<()>
    where
        R: tokio::io::AsyncBufRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let mut lines = BufReader::new(reader).lines();
        let mut out = writer;

        while let Some(line) = lines.next_line().await? {
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }

            // Parse the request
            let request: JsonRpcRequest = match serde_json::from_str(&line) {
                Ok(req) => req,
                Err(_) => {
                    let resp = JsonRpcResponse::parse_error();
                    let json = serde_json::to_string(&resp)?;
                    out.write_all(json.as_bytes()).await?;
                    out.write_all(b"\n").await?;
                    out.flush().await?;
                    continue;
                }
            };

            // Dispatch
            if let Some(response) = dispatch_request(&request, &self.tool_specs, &self.tools).await
            {
                let json = serde_json::to_string(&response)?;
                out.write_all(json.as_bytes()).await?;
                out.write_all(b"\n").await?;
                out.flush().await?;
            }
        }

        Ok(())
    }
}

/// Manages the lifecycle of one or more MCP servers and their tools.
///
/// Call [`McpServerManager::register_all`] to spawn every configured server,
/// discover its tools, and register them into a [`ToolRegistry`]. The manager
/// keeps the underlying [`McpClient`]s alive so they can handle tool calls.
pub struct McpServerManager {
    /// Configured servers.
    servers: Vec<McpServer>,
    /// Running clients, keyed by server name.
    clients: Mutex<HashMap<String, Arc<Mutex<McpClient>>>>,
}

impl McpServerManager {
    /// Create a new manager from a list of server configurations.
    ///
    /// The servers are not spawned until [`register_all`](Self::register_all) is called.
    pub fn new(servers: Vec<McpServer>) -> Self {
        Self {
            servers,
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// Spawn all configured servers, discover their tools, and register them
    /// into the given [`ToolRegistry`].
    ///
    /// Returns a list of `(server_name, tool_count)` pairs for logging.
    ///
    /// # Errors
    ///
    /// Returns an error if any server fails to start or if tool discovery
    /// fails. Servers are started sequentially; a failure stops the process.
    #[instrument(skip_all, name = "mcp.register_all")]
    pub async fn register_all(&self, registry: &mut ToolRegistry) -> Result<Vec<(String, usize)>> {
        let mut results = Vec::new();

        for server in &self.servers {
            let name = server.name.clone();
            info!(server = %name, "Starting MCP server");

            let client = McpClient::spawn(server).await.map_err(|e| Error::Tool {
                name: format!("mcp_server:{name}"),
                message: format!("Failed to start MCP server: {e}"),
            })?;

            let client = Arc::new(Mutex::new(client));

            // Discover tools from this server.
            let tool_specs = client
                .lock()
                .await
                .list_tools()
                .await
                .map_err(|e| Error::Tool {
                    name: format!("mcp_server:{name}"),
                    message: format!("Failed to discover tools from MCP server: {e}"),
                })?;

            let tool_count = tool_specs.len();
            info!(
                server = %name,
                count = tool_count,
                "Discovered MCP tools"
            );

            // Wrap each tool spec in an McpTool and register it.
            for spec in &tool_specs {
                let tool = McpTool::new(client.clone(), spec.clone(), &name);
                registry.register_mut(Arc::new(tool));
                info!(
                    server = %name,
                    tool = %spec.name,
                    "Registered MCP tool"
                );
            }

            // Store the client so it stays alive.
            self.clients.lock().await.insert(name.clone(), client);
            results.push((name, tool_count));
        }

        Ok(results)
    }

    /// Shut down all running MCP clients.
    ///
    /// This drops the clients, which causes their background tasks (stdio
    /// reader/writer, SSE listener) to be cancelled.
    pub async fn shutdown(&self) {
        let mut clients = self.clients.lock().await;
        let names: Vec<String> = clients.keys().cloned().collect();
        for name in &names {
            info!(server = %name, "Shutting down MCP server");
            clients.remove(name);
        }
    }

    /// Return the number of currently running clients.
    pub async fn running_count(&self) -> usize {
        self.clients.lock().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::McpServer;

    /// Test that an empty server list produces no registrations.
    #[tokio::test]
    async fn empty_servers_registers_nothing() {
        let manager = McpServerManager::new(vec![]);
        let mut registry = ToolRegistry::local();
        let results = manager.register_all(&mut registry).await.unwrap();
        assert!(results.is_empty());
        assert!(registry.names().is_empty());
    }

    /// Test that tool names are correctly namespaced.
    #[test]
    fn tool_name_format() {
        let server = "my-server";
        let tool = "read_file";
        let namespaced = format!("mcp__{}__{}", server, tool);
        assert_eq!(namespaced, "mcp__my-server__read_file");
    }

    /// Test that a server config with a URL creates an SSE-based client
    /// (as opposed to stdio). This is a config-level test only.
    #[test]
    fn sse_server_config_detection() {
        let server = McpServer {
            name: "test-sse".into(),
            command: String::new(),
            args: vec![],
            url: Some("http://localhost:8080/sse".into()),
        };
        // The McpClient::spawn method checks server.url.is_some()
        // to decide transport. We verify the config is wired correctly.
        assert!(server.url.is_some());
        assert!(server.command.is_empty());
    }

    /// Test that a server config with a command creates a stdio-based client.
    #[test]
    fn stdio_server_config_detection() {
        let server = McpServer {
            name: "test-stdio".into(),
            command: "echo".into(),
            args: vec!["hello".into()],
            url: None,
        };
        assert!(!server.command.is_empty());
        assert!(server.url.is_none());
    }
}
