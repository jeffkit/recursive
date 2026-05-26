# Goal 91 — HTTP API: axum server scaffold + /tools endpoint

**Roadmap**: Phase 12.1 — HTTP API (part 1/6)

**Design principle check**:
- Implemented as: new feature-gated module `src/http.rs` + new feature `http`
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- Orthogonal: HTTP layer imports from lib but doesn't change core

## Why

Recursive needs an HTTP API for external integration — TUI, SDKs, and
third-party tools will communicate through it. The first step is a
minimal axum server that exposes the tool registry as a read-only JSON
endpoint, proving the wiring works.

## Scope (do exactly this, no more)

### 1. `Cargo.toml` — add `http` feature + dependencies

```toml
http = ["dep:axum", "dep:tower-http"]

[dependencies]
axum = { version = "0.8", optional = true }
tower-http = { version = "0.6", features = ["cors", "trace"], optional = true }
```

Also add `http` to the `default` features list.

### 2. `src/http.rs` — new file

```rust
//! HTTP API server for Recursive agent.

use axum::{extract::State, routing::get, Json, Router};
use std::sync::Arc;

/// Shared application state for the HTTP server.
#[derive(Clone)]
pub struct AppState {
    pub tools: Vec<ToolInfo>,
}

/// Serializable tool info for the /tools endpoint.
#[derive(Clone, serde::Serialize)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Build the axum Router with all API routes.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/tools", get(list_tools))
        .with_state(Arc::new(state))
}

async fn health() -> &'static str {
    "ok"
}

async fn list_tools(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<ToolInfo>> {
    Json(state.tools.clone())
}
```

### 3. `src/lib.rs` — add module declaration

```rust
#[cfg(feature = "http")]
pub mod http;
```

### 4. `src/main.rs` — add `Serve` subcommand

Add a `Serve` variant to the CLI enum:

```rust
/// Start the HTTP API server.
Serve {
    /// Address to bind (default: 127.0.0.1:3000)
    #[arg(long, default_value = "127.0.0.1:3000")]
    addr: String,
},
```

Handler:
```rust
Cmd::Serve { addr } => {
    let registry = build_tool_registry(&config);
    let tools: Vec<http::ToolInfo> = registry.list().iter().map(|spec| {
        http::ToolInfo {
            name: spec.name.clone(),
            description: spec.description.clone(),
            parameters: spec.parameters.clone(),
        }
    }).collect();
    let state = http::AppState { tools };
    let router = http::build_router(state);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("Recursive HTTP API listening on {addr}");
    axum::serve(listener, router).await?;
}
```

### 5. Tests

Add to a new `tests/http.rs` integration test file:

- Test: `/health` returns 200 "ok"
- Test: `/tools` returns JSON array
- Test: `/tools` contains expected tool names from default registry

Use `axum::extract::connect_info` or `tower::ServiceExt` to test
without binding a real port.

## Acceptance

- `cargo test` green (all existing + new http tests)
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo build --features http` compiles
- `recursive serve --help` shows usage

## Notes for the agent

- Read `src/main.rs` for existing Clap subcommand structure (Run, Repl, Loop, MCP Server).
- Read `src/tools/mod.rs` for `ToolRegistry::list()` → returns `Vec<ToolSpec>`.
- Read `src/mcp_server.rs` for how another "server mode" was wired — similar pattern.
- The `ToolSpec` struct has `name: String`, `description: String`,
  `parameters: serde_json::Value` — map directly to `ToolInfo`.
- Use `axum 0.8` (latest stable). Router state is `Arc<AppState>`.
- For tests, use `tower::ServiceExt` + `axum::body::Body` to send
  requests without a real TCP listener.
- **DO NOT modify `src/agent.rs` or any tool file.**
- **DO NOT add authentication or middleware beyond basic CORS.**
