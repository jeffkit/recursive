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
pub mod checkpoint;
pub mod checkpoint_log;
pub mod compact;
pub mod config;
pub mod config_file;
pub mod cost;
pub mod error;
pub mod event;
pub mod hooks;
#[cfg(feature = "http")]
pub mod http;
pub mod kernel;
pub mod llm;
#[cfg(feature = "mcp")]
pub mod mcp;
#[cfg(feature = "mcp")]
pub mod mcp_server;
pub mod message;
pub mod migrate;
pub mod multi;
pub mod paths;
pub mod permissions;
pub mod rewind;
pub mod runtime;
pub mod runtime_goal;
pub mod session;
pub mod session_lock;
pub mod skills;
pub mod tools;
pub mod transcript;
#[cfg(feature = "tui")]
pub mod tui;

pub use agent::PlanningMode;
pub use agent::{FinishReason, PermissionDecision, PermissionHook};
// Legacy API — kept for backward compatibility; new code should use AgentRuntime.
#[allow(deprecated)]
pub use agent::{Agent, AgentOutcome, OnMessageFn, StepEvent};
pub use checkpoint::{CheckpointId, CheckpointInfo, RestoreStats, ShadowRepo};
pub use checkpoint_log::{
    read_log as read_checkpoint_log, truncate_to_turn as truncate_checkpoint_log,
    CheckpointLogWriter, CheckpointRecord, TouchedVia,
};
pub use compact::Compactor;
pub use config::Config;
pub use error::{Error, Result};
pub use event::{AgentEvent, ChannelSink, CompositeSink, EventSink, NullSink};
pub use hooks::ToolTimingHook;
pub use hooks::{Hook, HookAction, HookEvent, HookRegistry};
pub use kernel::{AgentKernel, AgentKernelBuilder, SideEffect, TurnContext, TurnOutcome};
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
pub use migrate::{migrate_workspace, MigrateReport};
pub use multi::{
    parse_delegations, AgentMessage, AgentPool, AgentRole, DelegationResult, MemoryEntry,
    MessageBus, MessageType, Pipeline, PipelineResult, SharedMemory, StageOutcome,
    TeamOrchestrator, TeamResult,
};
pub use paths::{
    legacy_paths_in_workspace, user_data_dir, user_scratchpad_path, user_sessions_dir,
    user_shadow_git_dir, user_workspace_dir, workspace_hash,
};
pub use rewind::{
    apply_rewind, checkpoint_log_path, detect_conflicts, plan_rewind, ConflictReport, RewindPlan,
    RewindResult,
};
pub use runtime::{AgentRuntime, AgentRuntimeBuilder, RuntimeOutcome};
pub use session::OrphanToolCall;
pub use session::SessionFile;
pub use session::SessionPersistenceSink;
pub use session::SessionReader;
pub use session::SessionWriter;
pub use session::{
    entry_to_message, truncate_transcript_to_turn, SessionLock, SessionMeta, TranscriptEntry,
    TruncateStats,
};
pub use skills::{
    discover_skills, skill_index, skills_for_injection, Skill, SkillMode, SkillParam, SkillRef,
    SkillScript, SkillSection,
};
pub use tools::{
    build_standard_tools, AuditMeta, EnterPlanModeTool, ExitPlanModeTool, ExitStatus,
    PlanApprovalGate, PlanApprovalResult, TodoItem, TodoStatus, TodoWriteTool, Tool, ToolDispatch,
    ToolRegistry, ToolSideEffect, TouchedFiles, AUDIT_ERR_MAX_BYTES,
};
pub use transcript::{TranscriptFile, TranscriptMeta};

#[cfg(any(test, feature = "test-utils"))]
pub mod test_util;

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
