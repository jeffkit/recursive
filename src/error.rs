//! Crate-wide error and Result.
//!
//! Structured error types that library consumers can match on.
//! Every distinct failure mode has its own variant.

use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum Error {
    /// LLM provider returned an error (HTTP, parse, etc.)
    #[error("LLM error ({provider}): {message}")]
    Llm { provider: String, message: String },

    /// LLM rate limited — caller should retry after `retry_after_ms`
    #[error("LLM rate limited ({provider}): retry after {retry_after_ms}ms")]
    RateLimited { provider: String, retry_after_ms: u64 },

    /// Tool execution failure (spawn, timeout, I/O)
    #[error("tool error ({name}): {message}")]
    Tool { name: String, message: String },

    /// Invalid arguments passed to a tool
    #[error("bad tool arguments ({name}): {message}")]
    BadToolArgs { name: String, message: String },

    /// Tool not found in registry
    #[error("tool `{0}` not found")]
    UnknownTool(String),

    /// Permission denied for a tool call
    #[error("permission denied: tool {name}")]
    PermissionDenied { name: String },

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

    /// Catch-all for errors that don't fit elsewhere
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Returns `true` if the error is safe to retry (rate limits, timeouts).
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Error::RateLimited { .. } | Error::Timeout { .. }
        )
    }

    /// Returns `true` if the error is transient (network issues, timeouts).
    /// Transient errors may resolve without changing the request.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Error::RateLimited { .. }
                | Error::Timeout { .. }
                | Error::Http(_)
                | Error::Io(_)
        )
    }
}

impl From<anyhow::Error> for Error {
    fn from(value: anyhow::Error) -> Self {
        Error::Other(value.to_string())
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
            name: "run_shell".into(),
            message: "command not found".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("run_shell"));
        assert!(msg.contains("command not found"));
    }

    #[test]
    fn test_bad_tool_args_format() {
        let err = Error::BadToolArgs {
            name: "read_file".into(),
            message: "missing path".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("read_file"));
        assert!(msg.contains("missing path"));
    }

    #[test]
    fn test_permission_denied_format() {
        let err = Error::PermissionDenied {
            name: "run_shell".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("run_shell"));
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
    fn test_from_anyhow_error() {
        let anyhow_err = anyhow::anyhow!("something went wrong");
        let err: Error = anyhow_err.into();
        assert!(matches!(err, Error::Other(_)));
        assert!(err.to_string().contains("something went wrong"));
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
}
