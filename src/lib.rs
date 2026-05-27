//! Recursive: a minimal, orthogonal, self-improving coding agent kernel.
//!
//! The kernel is intentionally tiny:
//!   - `Message` is the only data primitive shared across the system.
//!   - `LlmProvider` abstracts model backends (HTTP, mock, future local...).
//!   - `Tool` abstracts side effects the model can request.
//!   - `Agent` is a thin loop that wires them together.
//!
//! Everything else is opt-in. New capabilities are added by implementing
//! `Tool` or `LlmProvider`, never by editing the loop.

pub mod agent;
pub mod compact;
pub mod cost;
pub mod config;
pub mod config_file;
pub mod error;
pub mod hooks;
#[cfg(feature = "http")]
pub mod http;
pub mod llm;
#[cfg(feature = "mcp")]
pub mod mcp;
#[cfg(feature = "mcp")]
pub mod mcp_server;
pub mod message;
pub mod multi;
pub mod runner;
pub mod session;
pub mod skills;
pub mod tools;
pub mod transcript;

pub use agent::OnMessageFn;
pub use agent::PlanningMode;
pub use agent::{Agent, AgentOutcome, FinishReason, StepEvent};
pub use agent::{PermissionDecision, PermissionHook};
pub use compact::Compactor;
pub use config::Config;
pub use error::{Error, Result};
pub use hooks::ToolTimingHook;
pub use hooks::{Hook, HookAction, HookEvent, HookRegistry};
pub use llm::openai::RetryPolicy;
pub use llm::{pricing_for, Completion, LlmProvider, ModelPricing, TokenUsage, ToolCall, ToolSpec};
#[cfg(feature = "mcp")]
pub use mcp::{
    discover_mcp_servers, load_mcp_config, McpClient, McpPrompt, McpPromptArgument,
    McpPromptMessage, McpResource, McpResourceContent, McpServer, McpServerConfig, McpTool,
    McpToolSpec, ServerCapabilities,
};
#[cfg(feature = "mcp")]
pub use mcp_server::{McpServerManager, McpServerRunner};
pub use message::{Message, Role};
pub use multi::{
    parse_delegations, AgentMessage, AgentPool, AgentRole, DelegationResult, MemoryEntry,
    MessageBus, MessageType, Pipeline, PipelineResult, SharedMemory, StageOutcome,
    TeamOrchestrator, TeamResult,
};
pub use runner::AgentRunner;
pub use session::SessionFile;
pub use session::SessionReader;
pub use session::SessionWriter;
pub use skills::{
    discover_skills, skill_index, skills_for_injection, Skill, SkillMode, SkillParam, SkillRef,
    SkillScript, SkillSection,
};
pub use tools::{Tool, ToolRegistry};
pub use transcript::{TranscriptFile, TranscriptMeta};

/// Safely truncate a string to at most `max_bytes` bytes without splitting
/// a multi-byte UTF-8 character. Returns the full string if it's already
/// within the limit.
#[inline]
pub fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
