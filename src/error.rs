//! Crate-wide error and Result.
//!
//! Structured error types that library consumers can match on.
//! Every distinct failure mode has its own variant.

use crate::permissions::DecisionReason;
use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum Error {
    /// LLM provider returned an error (HTTP, parse, etc.)
    #[error("LLM error ({provider}): {message}")]
    Llm { provider: String, message: String },

    /// LLM rate limited — caller should retry after `retry_after_ms`
    #[error("LLM rate limited ({provider}): retry after {retry_after_ms}ms")]
    RateLimited {
        provider: String,
        retry_after_ms: u64,
    },

    /// Tool execution failure (spawn, timeout, I/O)
    ///
    /// `call_id` links this error back to the `tool_call_id` of the triggering
    /// `ToolCall`, enabling audit logs and session persistence to correlate the
    /// failure with the exact turn that requested the tool.
    #[error("tool error ({name}): {message}")]
    Tool {
        name: String,
        /// The tool_call_id from the triggering ToolCall, if available.
        call_id: Option<String>,
        message: String,
    },

    /// Invalid arguments passed to a tool
    #[error("bad tool arguments ({name}): {message}")]
    BadToolArgs { name: String, message: String },

    /// Tool rejected execution (policy, constraints, safety)
    #[error("tool rejected ({name}): {reason}")]
    ToolRejected { name: String, reason: String },

    /// Tool not found in registry
    #[error("tool `{0}` not found")]
    UnknownTool(String),

    /// Permission denied for a tool call
    #[error("permission denied: tool {name} ({reason:?})")]
    PermissionDenied {
        name: String,
        reason: DecisionReason,
    },

    /// Auto-classifier denial limit exceeded — agent should stop
    #[error("permission denial limit exceeded for tool {name}")]
    PermissionDeniedLimit { name: String },

    /// LLM response truncated by provider
    #[error("llm response truncated by provider (finish_reason = {0:?})")]
    ProviderTruncated(String),

    /// MCP protocol/transport error
    #[error("MCP error ({server}): {message}")]
    Mcp { server: String, message: String },

    /// Configuration error (missing env var, invalid value)
    #[error("config error: {message}")]
    Config { message: String },

    /// I/O error
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// HTTP client error
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// JSON serialization/deserialization error
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Timeout after a specified duration
    #[error("timeout after {duration_ms}ms")]
    Timeout { duration_ms: u64 },

    /// Storage backend error (I/O, serialization, remote storage, etc.)
    #[error("storage error: {message}")]
    Storage { message: String },

    /// A named resource (team, task, etc.) was requested but does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// Session metadata on disk has a `schema_version` newer than this
    /// build of the binary knows how to interpret. Raised by
    /// `SessionReader::load_meta` when it sees a version greater than
    /// the supported maximum — the safe action is to refuse to load
    /// the session, not to silently drop fields.
    #[error("session {session_id} has schema_version={found}, supported up to {supported}")]
    SchemaTooNew {
        session_id: String,
        found: u32,
        supported: u32,
    },

    /// Internal agent error — unexpected state that does not map to any typed
    /// variant. Prefer a specific variant; use `Internal` only as last resort.
    #[error("internal error ({context}): {message}")]
    Internal { context: String, message: String },
}

impl Error {
    /// Returns `true` if the error is safe to retry (rate limits, timeouts).
    pub fn is_retryable(&self) -> bool {
        matches!(self, Error::RateLimited { .. } | Error::Timeout { .. })
    }

    /// Returns `true` if the error is transient (network issues, timeouts).
    /// Transient errors may resolve without changing the request.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Error::RateLimited { .. } | Error::Timeout { .. } | Error::Http(_) | Error::Io(_)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_llm_error_format() {
        let err = Error::Llm {
            provider: "openai".into(),
            message: "rate limit hit".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("openai"));
        assert!(msg.contains("rate limit"));
    }

    #[test]
    fn test_rate_limited_format() {
        let err = Error::RateLimited {
            provider: "deepseek".into(),
            retry_after_ms: 5000,
        };
        let msg = err.to_string();
        assert!(msg.contains("deepseek"));
        assert!(msg.contains("5000"));
    }

    #[test]
    fn test_tool_error_format() {
        let err = Error::Tool {
            name: "Bash".into(),
            call_id: None,
            message: "command not found".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("Bash"));
        assert!(msg.contains("command not found"));
    }

    #[test]
    fn test_bad_tool_args_format() {
        let err = Error::BadToolArgs {
            name: "Read".into(),
            message: "missing path".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("Read"));
        assert!(msg.contains("missing path"));
    }

    #[test]
    fn test_permission_denied_format() {
        let err = Error::PermissionDenied {
            name: "Bash".into(),
            reason: crate::permissions::DecisionReason::Mode(
                crate::permissions::PermissionMode::DontAsk,
            ),
        };
        let msg = err.to_string();
        assert!(msg.contains("Bash"));
        assert!(msg.contains("DontAsk"));
    }

    #[test]
    fn test_mcp_error_format() {
        let err = Error::Mcp {
            server: "filesystem".into(),
            message: "connection refused".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("filesystem"));
        assert!(msg.contains("connection refused"));
    }

    #[test]
    fn test_config_error_format() {
        let err = Error::Config {
            message: "missing RECURSIVE_API_KEY".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("missing RECURSIVE_API_KEY"));
    }

    #[test]
    fn test_timeout_format() {
        let err = Error::Timeout { duration_ms: 30000 };
        let msg = err.to_string();
        assert!(msg.contains("30000"));
    }

    #[test]
    fn test_is_retryable() {
        assert!(Error::RateLimited {
            provider: "x".into(),
            retry_after_ms: 1000
        }
        .is_retryable());
        assert!(Error::Timeout { duration_ms: 5000 }.is_retryable());
        assert!(!Error::Tool {
            name: "x".into(),
            call_id: None,
            message: "fail".into()
        }
        .is_retryable());
        assert!(!Error::Config {
            message: "bad".into()
        }
        .is_retryable());
    }

    #[test]
    fn test_is_transient() {
        assert!(Error::RateLimited {
            provider: "x".into(),
            retry_after_ms: 1000
        }
        .is_transient());
        assert!(Error::Timeout { duration_ms: 5000 }.is_transient());
        // reqwest::Error is transient by definition (network issues).
        // We verify the variant match in the is_transient implementation itself.
        assert!(!Error::Tool {
            name: "x".into(),
            call_id: None,
            message: "fail".into()
        }
        .is_transient());
        assert!(!Error::Config {
            message: "bad".into()
        }
        .is_transient());
    }

    #[test]
    fn test_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err: Error = io_err.into();
        assert!(matches!(err, Error::Io(_)));
        assert!(err.to_string().contains("file not found"));
    }

    #[test]
    fn test_unknown_tool_format() {
        let err = Error::UnknownTool("nonexistent".into());
        let msg = err.to_string();
        assert!(msg.contains("nonexistent"));
    }

    #[test]
    fn test_provider_truncated_format() {
        let err = Error::ProviderTruncated("length".into());
        let msg = err.to_string();
        assert!(msg.contains("length"));
    }

    #[test]
    fn test_tool_rejected_format() {
        // kills `ToolRejected` match-arm replacement mutations
        let err = Error::ToolRejected {
            name: "Bash".into(),
            reason: "policy violation".into(),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("Bash"),
            "name must appear in ToolRejected display"
        );
        assert!(
            msg.contains("policy violation"),
            "reason must appear in ToolRejected display"
        );
    }

    #[test]
    fn test_permission_denied_limit_format() {
        // kills `PermissionDeniedLimit` match-arm replacement mutations
        let err = Error::PermissionDeniedLimit {
            name: "Write".into(),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("Write"),
            "name must appear in PermissionDeniedLimit display"
        );
    }

    #[test]
    fn test_schema_too_new_format() {
        // kills `SchemaTooNew` match-arm replacement mutations
        let err = Error::SchemaTooNew {
            session_id: "sess-123".into(),
            found: 5,
            supported: 2,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("sess-123"),
            "session_id must appear in SchemaTooNew display"
        );
        assert!(msg.contains("5"), "found version must appear");
        assert!(msg.contains("2"), "supported version must appear");
    }

    #[test]
    fn test_internal_error_format() {
        // kills `Internal` match-arm replacement mutations
        let err = Error::Internal {
            context: "run_inner".into(),
            message: "unexpected None".into(),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("run_inner"),
            "context must appear in Internal display"
        );
        assert!(
            msg.contains("unexpected None"),
            "message must appear in Internal display"
        );
    }

    #[test]
    fn test_storage_error_format() {
        let err = Error::Storage {
            message: "disk full".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("disk full"));
    }

    #[test]
    fn test_not_found_format() {
        let err = Error::NotFound("task-abc".into());
        let msg = err.to_string();
        assert!(msg.contains("task-abc"));
    }

    #[test]
    fn test_is_retryable_for_non_retryable_variants() {
        // kills mutations that make non-retryable errors retryable
        assert!(!Error::UnknownTool("Bash".into()).is_retryable());
        assert!(!Error::BadToolArgs {
            name: "r".into(),
            message: "m".into()
        }
        .is_retryable());
    }

    #[test]
    fn test_is_transient_for_non_transient_variants() {
        // kills mutations that expand the is_transient match arms
        assert!(!Error::UnknownTool("Bash".into()).is_transient());
        assert!(!Error::Config {
            message: "bad config".into()
        }
        .is_transient());
    }
}
