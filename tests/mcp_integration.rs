//! Integration tests against a real MCP filesystem server.
//!
//! These tests spawn the `@modelcontextprotocol/server-filesystem` MCP server
//! via `npx` and exercise the full MCP client lifecycle:
//!
//!   A. Initialize + list tools
//!   B. Read a real file
//!   C. Write a file
//!   D. Error handling (read nonexistent file)
//!
//! All tests are marked `#[ignore]` because they require `npx` on `$PATH`
//! and network access to download the npm package on first run.
//!
//! Run with:  cargo test --test mcp_integration -- --ignored

use recursive::mcp::{McpClient, McpServer};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tempfile::TempDir;

/// Shared MCP server config for all tests.
static SERVER: OnceLock<McpServer> = OnceLock::new();

/// Resolve a path to its canonical form, so the MCP filesystem server's
/// access-control check (which compares canonical paths) works correctly.
fn canonicalize(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// The directory the MCP server is allowed to serve. We canonicalize it so
/// that the server's internal path comparison works correctly on systems
/// where `/tmp` is a symlink (e.g. macOS: /tmp -> private/tmp).
fn allowed_dir() -> PathBuf {
    canonicalize(Path::new("/tmp"))
}

fn server() -> &'static McpServer {
    SERVER.get_or_init(|| {
        let dir = allowed_dir();
        McpServer {
            name: "filesystem-test".into(),
            command: "npx".into(),
            args: vec![
                "-y".into(),
                "@modelcontextprotocol/server-filesystem".into(),
                dir.to_string_lossy().into(),
            ],
            url: None,
            env: None,
        }
    })
}

/// Create a temp directory under the allowed dir. Returns the TempDir
/// (keeps it alive) and the canonical path.
fn tmp_dir() -> (TempDir, PathBuf) {
    let tmp = TempDir::new_in(allowed_dir()).unwrap();
    let path = canonicalize(tmp.path());
    (tmp, path)
}

// ============================================================================
// Test A: Initialize + list tools
//
// Verifies that the MCP handshake succeeds and that the server advertises
// the expected filesystem tools.
// ============================================================================
#[tokio::test]
#[ignore]
async fn test_initialize_and_list_tools() {
    let mut client = McpClient::spawn(server()).await.unwrap();

    let tools = client.list_tools().await.unwrap();

    // The filesystem server should advertise at least these tools.
    let tool_names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(
        tool_names.contains(&"read_file"),
        "expected read_file tool, got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"write_file"),
        "expected write_file tool, got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"list_directory"),
        "expected list_directory tool, got: {tool_names:?}"
    );

    // Each tool should have a non-empty description and an input_schema.
    for tool in &tools {
        assert!(
            !tool.description.is_empty(),
            "tool '{}' has empty description",
            tool.name
        );
        assert!(
            tool.input_schema.is_object(),
            "tool '{}' has non-object input_schema",
            tool.name
        );
    }
}

// ============================================================================
// Test B: Read a real file
//
// Creates a temp file on disk, then reads it via the MCP filesystem server.
// ============================================================================
#[tokio::test]
#[ignore]
async fn test_read_file() {
    let (_tmp, dir) = tmp_dir();
    let file_path = dir.join("hello.txt");
    let expected_content = "Hello, MCP integration test!";
    std::fs::write(&file_path, expected_content).unwrap();

    let mut client = McpClient::spawn(server()).await.unwrap();

    let content = client
        .call_tool("read_file", json!({"path": file_path.to_string_lossy()}))
        .await
        .unwrap();

    assert_eq!(
        content, expected_content,
        "read_file returned unexpected content"
    );
}

// ============================================================================
// Test C: Write a file
//
// Writes a file via the MCP filesystem server, then verifies it exists on
// disk with the correct content.
// ============================================================================
#[tokio::test]
#[ignore]
async fn test_write_file() {
    let (_tmp, dir) = tmp_dir();
    let file_path = dir.join("written-by-mcp.txt");
    let content_to_write = "Written via MCP tool call!";

    let mut client = McpClient::spawn(server()).await.unwrap();

    let result = client
        .call_tool(
            "write_file",
            json!({
                "path": file_path.to_string_lossy(),
                "content": content_to_write,
            }),
        )
        .await
        .unwrap();

    // The filesystem server returns the path as confirmation.
    assert!(
        result.contains(file_path.to_string_lossy().as_ref()),
        "write_file response should contain the file path, got: {result}"
    );

    // Verify the file was actually written to disk.
    let on_disk = std::fs::read_to_string(&file_path).unwrap();
    assert_eq!(on_disk, content_to_write, "file content mismatch");
}

// ============================================================================
// Test D: Error handling
//
// Attempts to read a nonexistent file and verifies that the MCP client
// returns an `Error::Tool` with a descriptive message.
// ============================================================================
#[tokio::test]
#[ignore]
async fn test_read_nonexistent_file() {
    let (_tmp, dir) = tmp_dir();
    let nonexistent = dir.join("does-not-exist.txt");

    let mut client = McpClient::spawn(server()).await.unwrap();

    let err = client
        .call_tool("read_file", json!({"path": nonexistent.to_string_lossy()}))
        .await
        .unwrap_err();

    let err_str = err.to_string();
    assert!(
        err_str.contains("ENOENT")
            || err_str.contains("No such file")
            || err_str.contains("not found")
            || err_str.contains("does not exist"),
        "expected filesystem error for nonexistent file, got: {err_str}"
    );
}
