//! Recursive: a minimal, orthogonal, self-improving coding agent kernel.
//!
//! The kernel is intentionally tiny:
//!   - `Message` is the only data primitive shared across the system.
//!   - `ChatProvider` abstracts model backends (HTTP, mock, future local...).
//!   - `Tool` abstracts side effects the model can request.
//!   - `Agent` is a thin loop that wires them together.
//!
//! Everything else is opt-in. New capabilities are added by implementing
//! `Tool` or `ChatProvider`, never by editing the loop.
//!
//! # Lint policy
//!
//! `unwrap()` / `expect()` are banned in production code (AGENTS.md invariant #5).
//! Test code relaxes this via `#![cfg_attr(test, allow(...))]` below.
//! Any remaining production exception carries `#[allow(..., reason = "...")]`.
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod acp;
pub mod agent;
pub mod atomic;
pub mod checkpoint;
pub mod checkpoint_log;
pub mod compact;
pub mod config;
pub mod config_file;
pub mod coordinator;
pub mod cost;
pub mod error;
pub mod event;
pub mod hooks;
#[cfg(feature = "http")]
pub mod http;
pub mod kernel;
pub mod llm;
pub mod logging;
#[cfg(feature = "mcp")]
pub mod mcp;
#[cfg(feature = "mcp")]
pub mod mcp_server;
pub mod memory;
pub mod message;
pub mod migrate;
pub mod multi;
pub mod paths;
pub mod permissions;
pub mod providers;
pub mod providers_cache;
pub mod rewind;
pub mod run_core;
pub mod runtime;
pub mod runtime_goal;
pub mod session;
pub mod skills;
pub mod skills_injector;
pub mod storage;
pub mod system_prompt;
pub mod tasks;
pub mod team;
pub mod tool_set_provider;
pub mod tools;
pub mod transcript;
#[cfg(feature = "weixin")]
pub mod weixin;

pub use agent::{FinishReason, PermissionDecision};
pub use checkpoint::{CheckpointId, CheckpointInfo, RestoreStats, ShadowRepo};
pub use checkpoint_log::{
    read_log as read_checkpoint_log, truncate_to_turn as truncate_checkpoint_log,
    CheckpointLogWriter, CheckpointRecord, TouchedVia,
};
pub use compact::Compactor;
pub use config::Config;
pub use error::{Error, Result};
pub use event::{AgentEvent, ChannelSink, CompositeSink, EventSink, NullSink};
pub use hooks::ExternalHookRunner;
pub use hooks::ToolTimingHook;
pub use hooks::{Hook, HookAction, HookEvent, HookRegistry, SdkHookForwarder};
pub use kernel::{AgentKernel, AgentKernelBuilder, TurnContext, TurnOutcome};
pub use llm::{
    context_window_tokens_for_model, default_compact_threshold_chars, pricing_for, ChatProvider,
    Completion, ModelPricing, RetryPolicy, TokenUsage, ToolCall, ToolSpec,
};
#[cfg(feature = "mcp")]
pub use mcp::{
    discover_mcp_servers, load_mcp_config, new_elicitation_slot, ElicitationHandler,
    ElicitationRequest, McpClient, McpPrompt, McpPromptArgument, McpPromptMessage, McpResource,
    McpResourceContent, McpServer, McpServerConfig, McpTool, McpToolSpec, ServerCapabilities,
    SharedElicitationHandler,
};
#[cfg(feature = "mcp")]
pub use mcp_server::{McpServerManager, McpServerRunner};
pub use message::{Message, Role};
pub use migrate::{migrate_workspace, MigrateReport};
pub use multi::{
    coordinator_system_prompt, register_subagent_if_enabled, AgentMessage, AgentPool, AgentRole,
    MemoryEntry, MessageBus, MessageType, SharedMemory,
};
pub use paths::{
    legacy_paths_in_workspace, user_data_dir, user_scratchpad_path, user_sessions_dir,
    user_shadow_git_dir, user_workspace_dir, workspace_hash,
};
pub use permissions::auto_classifier::{AutoClassifier, DenialTracker};
pub use permissions::PermissionMode;
pub use permissions::{LayeredPermissionsConfig, PermissionLayer, RuleSource};
pub use permissions::{RuleBehavior, SharedPermissions};
pub use providers::{
    all_presets, all_presets_dynamic, all_presets_effective, find_preset, find_preset_effective,
    find_preset_extended, ModelSpec, ProviderPreset,
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
pub use session::SessionStatus;
pub use session::SessionWriter;
pub use session::{
    entry_to_message, truncate_transcript_to_turn, SessionLock, SessionMeta, TranscriptEntry,
    TruncateStats,
};
pub use skills::{
    discover_skills, skill_index, skills_for_injection, Skill, SkillMode, SkillParam, SkillRef,
    SkillScript, SkillSection,
};
#[cfg(feature = "cloud-runtime")]
pub use storage::RedisSessionStore;
#[cfg(feature = "cloud-runtime")]
pub use storage::S3StorageBackend;
pub use storage::{
    AgentCheckpointState, LocalStorageBackend, NoopSessionStore, SessionStore, StorageBackend,
};
pub use system_prompt::assemble_system_prompt;
pub use tool_set_provider::{
    LocalToolSetProvider, PolicyToolSetProvider, SandboxMode, ToolSetProvider,
};
pub use tools::policy_sandbox::{FsPolicy, PolicyConfig, ShellPolicy};
pub use tools::PermissionHook;
pub use tools::{
    build_standard_tools, build_standard_tools_with_roots, new_shared_sandbox_roots, AccessTier,
    AuditMeta, EnterPlanModeTool, ExitPlanModeTool, ExitStatus, PlanApprovalGate,
    PlanApprovalResult, PlanModeRequestGate, PlanModeRequestResult, ReadFileState,
    RequestPlanModeTool, SharedSandboxRoots, TodoItem, TodoStatus, TodoWriteTool, Tool,
    ToolDispatch, ToolRegistry, ToolSideEffect, TouchedFiles, AUDIT_ERR_MAX_BYTES,
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
    let end = max_bytes.min(s.len());
    &s[..rewind_to_char_boundary(s, end)]
}

/// Walk `end` back to a UTF-8 char boundary.
///
/// `end > 0` is behavior-equivalent to `end >= 0` for `usize` (index 0 is
/// always a boundary), so the comparison mutant is skipped rather than
/// chased with an unobservable test.
#[cfg_attr(test, mutants::skip)]
fn rewind_to_char_boundary(s: &str, mut end: usize) -> usize {
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::truncate_str;

    #[test]
    fn truncate_str_short_string_returns_unchanged() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_str_exact_length_returns_unchanged() {
        assert_eq!(truncate_str("hello", 5), "hello");
    }

    #[test]
    fn truncate_str_over_limit_returns_prefix() {
        assert_eq!(truncate_str("hello world", 5), "hello");
    }

    #[test]
    fn truncate_str_empty_string() {
        assert_eq!(truncate_str("", 10), "");
    }

    #[test]
    fn truncate_str_zero_max_returns_empty() {
        assert_eq!(truncate_str("hello", 0), "");
    }

    #[test]
    fn truncate_str_multibyte_does_not_split_char() {
        // "日" is 3 bytes (UTF-8: 0xE6 0x97 0xA5)
        // "日本語" = 9 bytes; truncating at 4 bytes must not split 0xE6 0x97 (non-boundary)
        let s = "日本語";
        let truncated = truncate_str(s, 4);
        // Valid UTF-8: must be 3 bytes (one full "日") or 0 bytes
        assert!(s.is_char_boundary(truncated.len()));
        assert_eq!(truncated, "日");
    }

    #[test]
    fn truncate_str_ascii_one_byte_over() {
        assert_eq!(truncate_str("abcdef", 5), "abcde");
    }
}
