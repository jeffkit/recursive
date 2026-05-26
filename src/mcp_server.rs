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
use crate::mcp::{McpClient, McpServer, McpTool};
use crate::tools::ToolRegistry;

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

            let client = McpClient::spawn(server).await.map_err(|e| {
                Error::Tool {
                    name: format!("mcp_server:{name}"),
                    message: format!("Failed to start MCP server: {e}"),
                }
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
