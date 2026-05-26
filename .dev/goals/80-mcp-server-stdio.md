# Goal 80 — MCP Server: Stdio Transport Loop

**Roadmap**: Phase 6.3 part 2/3 — Stdio server loop

**Design principle check**:
- Implemented as: addition to `src/mcp_server.rs`. Uses the protocol
  types from g79. No agent loop changes.

## Why

This goal adds the actual IO loop that reads JSON-RPC from stdin and
writes responses to stdout. It builds on g79's dispatch_request function.

## Scope (do exactly this, no more)

### 1. `src/mcp_server.rs` — add `McpServerRunner`

```rust
pub struct McpServerRunner {
    tool_specs: Vec<ToolSpec>,
    tools: ToolRegistry,
}

impl McpServerRunner {
    pub fn new(tools: ToolRegistry) -> Self {
        let tool_specs = tools.specs();
        Self { tool_specs, tools }
    }

    /// Run the stdio server loop until EOF on stdin.
    pub async fn run(&self) -> Result<()> {
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        let reader = tokio::io::BufReader::new(stdin);

        let mut lines = reader.lines();
        while let Some(line) = lines.next_line().await? {
            let line = line.trim().to_string();
            if line.is_empty() { continue; }

            // Parse the request
            let request: JsonRpcRequest = match serde_json::from_str(&line) {
                Ok(req) => req,
                Err(_) => {
                    let resp = JsonRpcResponse::parse_error();
                    self.write_response(&stdout, &resp).await?;
                    continue;
                }
            };

            // Dispatch
            if let Some(response) = dispatch_request(&request, &self.tool_specs, &self.tools).await {
                self.write_response(&stdout, &response).await?;
            }
        }
        Ok(())
    }

    async fn write_response(&self, stdout: &tokio::io::Stdout, resp: &JsonRpcResponse) -> Result<()> {
        let json = serde_json::to_string(resp)?;
        let mut out = stdout.lock().await;
        out.write_all(json.as_bytes()).await?;
        out.write_all(b"\n").await?;
        out.flush().await?;
        Ok(())
    }
}
```

### 2. Key design rules

- Each JSON-RPC message is ONE LINE (newline-delimited JSON)
- stdout ONLY contains JSON-RPC responses — debug/logs go to stderr
- EOF on stdin means the client disconnected → exit gracefully
- Empty lines are skipped

### 3. Tests

Testing stdin/stdout is hard. Use `tokio::io::duplex` or byte buffers:

- Test: feed a valid request line, verify response line is valid JSON-RPC
- Test: feed invalid JSON, verify parse error response
- Test: feed multiple requests, verify multiple responses
- Test: feed a notification (no id), verify no response written
- Test: EOF terminates cleanly

For testing, refactor `run()` to accept generic `AsyncRead + AsyncWrite`
instead of hardcoded stdin/stdout:

```rust
pub async fn run_on<R, W>(&self, reader: R, writer: W) -> Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
```

Then `run()` just calls `run_on(stdin, stdout)`.

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets -- -D warnings` clean
- Server loop processes requests and writes responses
- Testable without actual stdin/stdout (generic reader/writer)
- g79's protocol layer used correctly

## Notes for the agent

- Read `src/mcp_server.rs` for the g79 types (JsonRpcRequest, dispatch_request).
- Use `tokio::io::BufReader::new(reader).lines()` for line-by-line reading.
- For tests: use `tokio_test::io::Builder` or just `std::io::Cursor` wrapped
  in a tokio adapter. Simplest: `&[u8]` implements AsyncRead.
- Don't forget `use tokio::io::{AsyncBufReadExt, AsyncWriteExt};`
- Keep it under 150 LOC added (the loop itself is simple).
