# Manual journal — 2026-10-04

**Goal:** #104 — close the next-layer MCP client gaps (remote auth, protocol
pin, unbounded tool results, container-tier MCP loss).

## Files touched

- `src/mcp.rs`
  - `McpServerConfig` / `McpServer`: new `headers` (auth) and `transport`
    fields; `headers` values support `${VAR}` env expansion (`expand_env`).
  - `McpClient::build_http_client` applies configured headers as reqwest
    default headers (covers both legacy SSE GET/POST and Streamable HTTP).
  - New `StreamableHttp` transport (2025-03-26): single-endpoint POST,
    `Mcp-Session-Id` capture/echo, JSON or SSE-in-POST response parsing
    (`parse_http_jsonrpc_response` / `parse_jsonrpc_message`).
  - `transport: Option<String>` (`auto` | `sse` | `http`) with
    Streamable-HTTP-first auto fallback to legacy SSE.
  - `initialize` now offers `2025-03-26` (`MCP_PROTOCOL_VERSION`) and warns
    only for revisions outside `SUPPORTED_PROTOCOL_VERSIONS`.
  - `McpTool::execute` truncates results to `DEFAULT_MCP_MAX_OUTPUT_BYTES`
    (65536) via `truncate_mcp_output`; override with
    `McpTool::with_max_output_bytes`.
- `src/tools/registry.rs` — `Tool::mcp_server_name()` (default `None`) +
  `ToolRegistry::mcp_tools()`.
- `src/http/mod.rs` — container-tier `rebind_per_session_registry` re-attaches
  `base.mcp_tools()` onto the fresh registry (was a documented gap).
- Struct-literal call sites updated: `src/mcp_server.rs`,
  `crates/recursive-cli/src/cli/builder.rs`, `crates/recursive-tui/src/backend.rs`,
  `tests/mcp_e2e.rs`, `tests/mcp_integration.rs`.

## Tests added

- `src/mcp.rs`: transport resolution, `${VAR}` expansion, discovery of
  headers/transport, truncation (boundary + multibyte), Streamable HTTP
  response parsing (object/batch/SSE/error), and two loopback end-to-end
  Streamable HTTP tests (initialize + tools/list, and auth-header emission).
- `src/tools/registry.rs`: `mcp_tools` excludes native tools / collects
  proxied tools.
- `src/http/mod.rs`: source-level guard that the container rebuild re-attaches
  MCP tools.
- `crates/recursive-tui/src/backend.rs`:
  `mcp_server_transport_ignores_remote_auth_and_transport_override` (pins that
  the new `headers`/`transport` fields do not perturb the `/mcp` display).

## Quality gates

- `cargo test --workspace --all-features` — green.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — green.
- `cargo fmt --all` — clean.
- `.dev/scripts/tui-test-presence.sh` — PASS (test-bearing change added).
- `tui-mutants.sh` scope: the only `recursive-tui/src` edit is inside `#[test]`
  functions, so `cargo mutants --in-diff` reports "No mutants to filter"
  (0 mutants) — the gate exits 0 without a long copy-mode run.

## Notes

- OAuth is supported via static `Authorization: Bearer ${TOKEN}` headers +
  env expansion; a full dynamic-registration/refresh flow is not implemented.
- `McpServer` gained two fields (breaking for external struct-literal
  constructors) — all in-repo sites updated.
