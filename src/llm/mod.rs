//! LLM provider abstraction.
//!
//! A provider takes a transcript plus tool specs and returns either
//! free-form content, structured tool calls, or both. The trait is the
//! only thing the agent depends on; everything beyond it (HTTP, retries,
//! mocking) lives in adapters.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Result;
use crate::message::Message;

pub mod mock;
pub mod openai;

pub use mock::MockProvider;
pub use openai::OpenAiProvider;

/// JSON-schema description of a tool, sent verbatim to the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema object describing the tool's input.
    pub parameters: Value,
}

/// A structured request to invoke one of the registered tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON arguments as produced by the model.
    pub arguments: Value,
}

/// One step of model output.
#[derive(Debug, Clone)]
pub struct Completion {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn complete(&self, messages: &[Message], tools: &[ToolSpec]) -> Result<Completion>;
}
