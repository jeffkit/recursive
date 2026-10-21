//! Issue #180: ACP sessions must give the agent a real tool surface.
//!
//! This file covers the `mcpServers` half end-to-end against the real
//! stdio transport: the configured server subprocess is spawned during
//! `session/new`, its tools are visible on the session's declared surface
//! (`result.tools`), and the subprocess is reaped when the session ends —
//! via `session/close` and via transport EOF (#183), the two reap points.
//!
//! The mock MCP server is the shared `mock_mcp_server` fixture (`echo`
//! mode, one tool named `echo`), resolved via the cargo-set
//! `CARGO_BIN_EXE_mock_mcp_server` env var. The server command is wrapped
//! in `sh -c 'echo $$ > <pidfile>; exec <mock> echo'` so the test learns
//! the child's OS pid and can observe its death; `exec` keeps the pid, so
//! the recorded pid *is* the mock server process.
//!
//! The fs-tools and permission-bridge halves live as unit tests in
//! `src/acp/server.rs` (they inspect the session registry directly).

#![cfg(all(unix, feature = "acp"))]

use std::sync::Arc;
use std::time::Duration;

use recursive::acp::server::AcpServer;
use recursive::llm::{ChatProvider, Completion, MockProvider};
use serde_json::Value;

/// A plain-text provider: no prompts are issued in these tests, but
/// `session/new` requires a provider to build the runtime.
fn plain_llm() -> Arc<dyn ChatProvider> {
    Arc::new(MockProvider::new(vec![Completion {
        content: "ok".into(),
        ..Default::default()
    }]))
}

/// Run the ACP server over an in-memory duplex: write `input`, signal EOF,
/// and collect every emitted frame as JSON.
async fn run_server(input: String) -> Vec<Value> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (mut client, server_io) = tokio::io::duplex(64 * 1024);
    let (server_r, server_w) = tokio::io::split(server_io);
    let server =
        tokio::spawn(async move { AcpServer::run_io(server_r, server_w, Some(plain_llm())).await });

    client
        .write_all(input.as_bytes())
        .await
        .expect("write input");
    client.shutdown().await.expect("signal EOF");

    let mut buf = Vec::new();
    client.read_to_end(&mut buf).await.expect("read output");
    server.await.expect("server task");

    String::from_utf8(buf)
        .expect("output is UTF-8")
        .lines()
        .map(|l| serde_json::from_str(l).expect("every frame is valid JSON"))
        .collect()
}

/// The `initialize` + `session/new` requests with one `sh`-wrapped mock
/// MCP server writing its pid to `pidfile`.
fn session_requests(cwd: &std::path::Path, pidfile: &std::path::Path, mock: &str) -> String {
    let script = format!("echo $$ > '{}'; exec '{}' echo", pidfile.display(), mock);
    format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1}}}}
{{"jsonrpc":"2.0","id":2,"method":"session/new","params":{{"cwd":"{cwd}","mcpServers":[{{"name":"mock","command":"sh","args":["-c","{script}"]}}]}}}}
"#,
        cwd = cwd.display(),
        script = script.replace('\\', "\\\\").replace('"', "\\\""),
    )
}

fn response_for(frames: &[Value], id: u64) -> Value {
    let target = Value::from(id);
    frames
        .iter()
        .find(|f| f.get("id") == Some(&target))
        .unwrap_or_else(|| panic!("no response with id {id} in: {frames:?}"))
        .clone()
}

/// Poll `ps` until the pid is gone or a zombie (dead, awaiting reap).
fn assert_process_dead(pid: u32) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let out = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output();
        let alive = match out {
            Ok(o) if o.status.success() => {
                let stat = String::from_utf8_lossy(&o.stdout);
                !(stat.trim().is_empty() || stat.trim().starts_with('Z'))
            }
            _ => false,
        };
        if !alive {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "MCP child {pid} must be reaped after the session ends"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn read_pid(pidfile: &std::path::Path) -> u32 {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(content) = std::fs::read_to_string(pidfile) {
            if let Ok(pid) = content.trim().parse::<u32>() {
                return pid;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "MCP server never wrote its pidfile at {}",
            pidfile.display()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `session/close` must reap the spawned MCP subprocess, and the session
/// surface must list the server's tool under its namespaced name.
#[tokio::test]
async fn acp_mcp_servers_are_spawned_and_reaped_on_close() {
    let tmp = tempfile::TempDir::new().unwrap();
    let pidfile = tmp.path().join("mock-close.pid");
    let mock = env!("CARGO_BIN_EXE_mock_mcp_server");

    let mut input = session_requests(tmp.path(), &pidfile, mock);
    input.push_str(
        r#"{"jsonrpc":"2.0","id":3,"method":"session/close","params":{"sessionId":"acp-sess-1"}}"#,
    );
    input.push('\n');

    let frames = run_server(input).await;

    // The spawned server's tool is on the session surface (spawn + handshake
    // + tools/list all succeeded during session/new).
    let new = response_for(&frames, 2);
    let tools: Vec<String> = new["result"]["tools"]
        .as_array()
        .expect("session/new reports tools")
        .iter()
        .map(|v| v.as_str().expect("tool name").to_string())
        .collect();
    assert!(
        tools.iter().any(|t| t == "mcp__mock__echo"),
        "session surface must contain the mock server's tool: {tools:?}"
    );

    // The child was started (pidfile written) and is gone after close.
    let pid = read_pid(&pidfile);
    assert!(pid > 0);
    assert_process_dead(pid);
}

/// Transport EOF (#183 shutdown path) must reap MCP subprocesses too —
/// a session that is never closed explicitly still releases its children.
#[tokio::test]
async fn acp_mcp_servers_are_reaped_on_eof() {
    let tmp = tempfile::TempDir::new().unwrap();
    let pidfile = tmp.path().join("mock-eof.pid");
    let mock = env!("CARGO_BIN_EXE_mock_mcp_server");

    let input = session_requests(tmp.path(), &pidfile, mock);
    let frames = run_server(input).await;

    let new = response_for(&frames, 2);
    assert!(
        new.get("result").is_some(),
        "session/new must succeed with mcpServers: {new}"
    );

    let pid = read_pid(&pidfile);
    assert_process_dead(pid);
}
