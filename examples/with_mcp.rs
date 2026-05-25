//! MCP (Model Context Protocol) integration: discover and connect to MCP
//! servers from the workspace. Requires a running MCP server configuration
//! (see `.mcp.json` or `.recursive/mcp.json` in the workspace root).
//!
//! This example is informational — it shows how to discover MCP servers and
//! connect to them. It will print a message if no MCP config is found.

use recursive::mcp::{discover_mcp_servers, McpClient, McpTool};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

#[tokio::main]
async fn main() {
    // Discover MCP servers from the current workspace.
    let workspace = Path::new(".");
    let servers = discover_mcp_servers(workspace)
        .await
        .expect("failed to discover MCP servers");

    if servers.is_empty() {
        println!("No MCP servers found. Create a `.mcp.json` file in the workspace root.");
        println!("Example `.mcp.json`:");
        println!("{{");
        println!("  \"mcpServers\": {{");
        println!("    \"my-server\": {{");
        println!("      \"command\": \"npx\",");
        println!("      \"args\": [\"-y\", \"@modelcontextprotocol/server-filesystem\", \".\"]");
        println!("    }}");
        println!("  }}");
        println!("}}");
        return;
    }

    println!("Found {} MCP server(s):", servers.len());

    for server in &servers {
        println!("\n  Server: {}", server.name);
        println!("  Command: {}", server.command);
        println!("  Args: {:?}", server.args);

        // Connect to the server.
        match McpClient::spawn(server).await {
            Ok(client) => {
                let client = Arc::new(Mutex::new(client));

                // List available tools from this server.
                let tools = {
                    let mut c = client.lock().await;
                    c.list_tools().await.unwrap_or_default()
                };

                println!("  Tools ({}):", tools.len());
                for tool in &tools {
                    println!("    - {}: {}", tool.name, tool.description);
                }

                // Wrap each tool for use with the agent.
                let _mcp_tools: Vec<_> = tools
                    .into_iter()
                    .map(|spec| McpTool::new(client.clone(), spec, &server.name))
                    .collect();

                println!("  ✓ Connected successfully");
            }
            Err(e) => {
                println!("  ✗ Failed to connect: {e}");
            }
        }
    }
}
