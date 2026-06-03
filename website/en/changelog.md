# Changelog

## v0.6.0 (current)

- Permission system v2: fine-grained per-tool permissions and approval flows
- `unwrap`-free codebase: all product code uses proper error propagation
- TUI improvements: markdown rendering, plan mode, scroll, session management
- Multi-agent enhancements: pipeline, team orchestration, shared memory bus
- Improved transcript compaction with LLM-driven summarisation

## v0.5.0

- **HTTP API** — axum-based REST server with sessions and SSE streaming
- **Terminal UI** — ratatui-based TUI with streaming tool indicators and plan mode
- **Multi-Agent** — agent pool, shared memory, messaging bus, pipeline & team orchestration
- **Python SDK** — `pip install recursive-client`
- **TypeScript SDK** — `npm install recursive-client`
- **Loop Mode** — `recursive loop` for self-scheduling autonomous agent runs

## v0.2.0

- Skill system v2: refs, scripts, params, injection modes, composition
- MCP HTTP+SSE transport
- MCP resources and prompts support
- Feature flags: `mcp`, `web_fetch`, `anthropic`
- Structured error types
- 5 runnable examples
- 367+ tests

## v0.1.0

- Minimal ReAct agent loop
- OpenAI-compatible LLM provider
- Filesystem tools: read, write, list, patch
- Shell tool with sandboxing and timeout
- Mock provider for offline testing
- CLI: `run`, `repl`, `tools` commands
- Hook system for lifecycle observation
- Transcript compaction
- MCP stdio transport
- Skill system v1
