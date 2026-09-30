//! Generic elicitation slot: a host-installable handler for protocol-level
//! elicitation requests (e.g. MCP `-32042` UrlElicitationRequired).
//!
//! The trait/type live here (core tools layer) so the tool registry can carry
//! the slot without depending on a protocol adapter; adapters (mcp) map from
//! these.

use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

/// Elicitation request forwarded to a host (e.g. MCP `-32042`
/// UrlElicitationRequired, Claude `elicitation` control).
#[derive(Debug, Clone)]
pub struct ElicitationRequest {
    pub mcp_server_name: String,
    pub message: String,
    pub mode: Option<String>,
    pub url: Option<String>,
    pub elicitation_id: Option<String>,
    pub requested_schema: Option<Value>,
    pub title: Option<String>,
    pub display_name: Option<String>,
    pub description: Option<String>,
}

/// Handles an elicitation request by asking the host.
#[async_trait]
pub trait ElicitationHandler: Send + Sync {
    async fn elicit(&self, request: ElicitationRequest) -> Option<Value>;
}

/// Shared slot so the host control channel can install a handler after
/// clients are constructed.
pub type SharedElicitationHandler = Arc<tokio::sync::RwLock<Option<Arc<dyn ElicitationHandler>>>>;

/// Create an empty elicitation-handler slot.
pub fn new_elicitation_slot() -> SharedElicitationHandler {
    std::sync::Arc::new(tokio::sync::RwLock::new(None))
}
