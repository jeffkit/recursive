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
pub mod config;
pub mod config_file;
pub mod error;
pub mod hooks;
pub mod llm;
#[cfg(feature = "mcp")]
pub mod mcp;
#[cfg(feature = "mcp")]
pub mod mcp_server;
#[cfg(feature = "http")]
pub mod http;
pub mod message;
pub mod multi;
pub mod runner;
pub mod session;
pub mod skills;
pub mod tools;
pub mod transcript;

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
    AgentMessage, AgentPool, AgentRole, MemoryEntry, MessageBus, MessageType, Pipeline,
    PipelineResult, SharedMemory, StageOutcome,
};
pub use runner::AgentRunner;
pub use session::SessionFile;
pub use skills::{
    discover_skills, skill_index, skills_for_injection, Skill, SkillMode, SkillParam, SkillRef,
    SkillScript, SkillSection,
};
pub use tools::{Tool, ToolRegistry};
pub use transcript::{TranscriptFile, TranscriptMeta};
