//! HTTP API server for the Recursive agent.
//!
//! Provides a lightweight axum-based HTTP server that exposes the agent's
//! tool registry as a read-only JSON endpoint, plus a health check.

use axum::{extract::State, routing::get, Json, Router};
use std::sync::Arc;

/// Shared application state for the HTTP server.
#[derive(Clone)]
pub struct AppState {
    pub tools: Vec<ToolInfo>,
}

/// Serializable tool info for the `/tools` endpoint.
#[derive(Clone, serde::Serialize, serde::Deserialize, Debug)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Build the axum [`Router`] with all API routes.
///
/// Routes:
/// - `GET /health` — returns `"ok"` (200)
/// - `GET /tools` — returns JSON array of [`ToolInfo`]
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/tools", get(list_tools))
        .with_state(Arc::new(state))
}

async fn health() -> &'static str {
    "ok"
}

async fn list_tools(State(state): State<Arc<AppState>>) -> Json<Vec<ToolInfo>> {
    Json(state.tools.clone())
}
