//! Crate-wide error and Result.
//!
//! Structured error types that library consumers can match on.
//! Every distinct failure mode has its own variant.

use crate::credentials::CredentialErrorCode;
use crate::permissions::DecisionReason;
use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
#[non_exhaustive]
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

    /// Credential resolution or authorization failure (issue #130).
    ///
    /// `code` is a stable, machine-readable discriminator (see
    /// [`CredentialErrorCode`]) so callers can tell a user *decline*
    /// (`authorization_declined`) from an infrastructure *failure*
    /// (`authorization_failed`) without matching the message text.
    #[error("credential error [{code}]: {message}")]
    Credential {
        code: CredentialErrorCode,
        message: String,
    },

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

    /// Operation cancelled via CancellationToken.
    #[error("cancelled")]
    Cancelled,

    /// Wall-clock budget exhausted (issue #40). Surfaced by the non-stream
    /// LLM call path so `run_inner` can finish with
    /// `FinishReason::WallClockExceeded` — finish is data, not an error
    /// (invariant #7), but it travels as an Error through the per-call path.
    #[error("wall-clock budget of {secs}s exceeded")]
    WallClockExceeded { secs: u64 },

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

    /// Workspace registry (issue #135): two distinct request paths realpath-
    /// resolve to the same canonical directory, and that canonical path is
    /// already owned by another registered workspace. Registering it again
    /// would alias two registry entries onto one directory, so it is refused.
    #[error("workspace realpath conflict: {canonical} is already registered as `{existing}`")]
    WorkspaceConflict { canonical: String, existing: String },

    /// Workspace registry (issue #135): the on-disk registry is internally
    /// inconsistent and no pending-mutation marker explains it (duplicate
    /// canonical paths, a malformed record, a corrupt index file). Recovery
    /// cannot proceed safely, so the state is reported loud as corrupt rather
    /// than silently repaired or ignored.
    #[error("workspace registry corrupt: {message}")]
    WorkspaceCorrupt { message: String },

    /// Archiving a workspace (issue #135) was refused because it still has
    /// active work — a live turn, a background job, a subagent, or a scheduled
    /// wakeup — and the caller asked for admission rather than
    /// stop-then-archive.
    #[error("workspace `{id}` has active work: {details}")]
    WorkspaceActiveWork { id: String, details: String },

    /// A session-tree export (issue #131) was refused because another export of
    /// the same session is already streaming: two concurrent downloads of a
    /// session whose writer is mid-append would each capture a different
    /// prefix.
    #[error("session {session_id} is already being exported")]
    ExportInProgress { session_id: String },

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

    /// The credential error code carried by this error, when it is one
    /// (issue #130).
    ///
    /// This is the branch point for credential handling: a decline and an
    /// infrastructure failure are different codes, not different message text.
    pub fn credential_code(&self) -> Option<CredentialErrorCode> {
        match self {
            Error::Credential { code, .. } => Some(*code),
            _ => None,
        }
    }

    /// HTTP status code carried by this error, when it came from an LLM
    /// HTTP response.
    ///
    /// The OpenAI / Anthropic adapters format an exhausted provider
    /// failure as `Error::Llm { message: "HTTP <status>: <body>" }`, so the
    /// code is parsed from that prefix; `Error::RateLimited` is the
    /// structured 429 variant. Transport failures, config errors and tool
    /// errors carry no status.
    pub fn http_status(&self) -> Option<u16> {
        match self {
            Error::RateLimited { .. } => Some(429),
            Error::Llm { message, .. } => message
                .strip_prefix("HTTP ")
                .and_then(|rest| rest.split_whitespace().next())
                .map(|code| code.trim_end_matches(':'))
                .and_then(|code| code.parse::<u16>().ok()),
            _ => None,
        }
    }

    /// Issue #123: did the LLM call itself fail *providing service* — a
    /// revoked key (401/403), a rate limit (429), a 5xx, an unreachable
    /// gateway, a malformed provider response?
    ///
    /// The adapters funnel all of those into [`Error::Llm`] (transport errors
    /// included, via `"request failed: …"`), so a message with no parseable
    /// status counts: the endpoint never answered.
    ///
    /// A **request-caused** 4xx does not — an oversized prompt earning a 400,
    /// or a 404 for a model name, says nothing about the endpoint's health,
    /// and three of those in a row would pull a healthy pod out of rotation.
    /// 401/403/429 are the opposite case: they reject the key or the account,
    /// so they stay provider-side.
    ///
    /// Drives `/readyz`'s LLM check, where a tool, storage or cancellation
    /// fault must not look like provider downtime.
    pub fn is_llm_failure(&self) -> bool {
        match self {
            Error::RateLimited { .. } => true,
            Error::Llm { .. } => match self.http_status() {
                Some(status) => !(400..500).contains(&status) || matches!(status, 401 | 403 | 429),
                None => true,
            },
            _ => false,
        }
    }

    /// Returns `true` for transport-level failures with no HTTP status to
    /// classify: a `reqwest` error, raw IO, a timeout, or the `Error::Llm`
    /// message the adapters synthesise for a dropped send / stream
    /// (`"request failed: …"` / `"SSE stream read error: …"`).
    pub fn is_network_error(&self) -> bool {
        match self {
            Error::Http(_) | Error::Io(_) | Error::Timeout { .. } => true,
            // Prefix match only: a parse failure embeds the raw response
            // body, which may itself contain words like "connection".
            Error::Llm { message, .. } => {
                message.starts_with("request failed:")
                    || message.starts_with("SSE stream read error:")
            }
            _ => false,
        }
    }

    /// Returns `true` when a bounded cross-step retry may recover from this
    /// provider failure: HTTP 429, any 5xx, or a network/transport error.
    /// Other 4xx (bad request, auth, context overflow) are permanent and
    /// must surface unchanged. Drives `RunCore`'s step-level retry
    /// (issue #100).
    pub fn is_transient_provider_error(&self) -> bool {
        self.is_network_error()
            || self
                .http_status()
                .is_some_and(|s| s == 429 || (500..600).contains(&s))
    }
}

/// Return `true` when `err` looks like an LLM context-window-exceeded error.
///
/// OpenAI-compatible providers (GLM, DeepSeek, …) return HTTP 400 whose body
/// contains an error code or human-readable message indicating the prompt is
/// too long. We match several common patterns across providers:
///
/// | Provider | Typical signal |
/// |---|---|
/// | OpenAI / NIM | `"context_length_exceeded"` (error `code`) |
/// | OpenAI / NIM | `"maximum context length"` (human message) |
/// | Some providers | `"context window"` |
/// | DeepSeek | `"prompt is too long"` |
/// | Some providers | `"tokens exceeds"` |
///
/// The function intentionally casts to lowercase before matching so it is
/// resilient to capitalisation differences across providers.
pub fn is_context_window_exceeded(err: &Error) -> bool {
    if let Error::Llm { message, .. } = err {
        let msg = message.to_lowercase();
        msg.contains("context_length_exceeded")
            || msg.contains("maximum context length")
            || msg.contains("context window")
            || msg.contains("prompt is too long")
            || msg.contains("tokens exceeds")
            || msg.contains("exceeds the model")
    } else {
        false
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
    fn test_cancelled_format() {
        let err = Error::Cancelled;
        let msg = err.to_string();
        assert!(msg.contains("cancelled"));
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

    // ── http_status / is_network_error / is_transient_provider_error ──────

    #[test]
    fn http_status_parses_adapter_llm_messages() {
        // The adapters format provider failures as "HTTP <status phrase>: body".
        let err = Error::Llm {
            provider: "openai".into(),
            message: "HTTP 429 Too Many Requests: slow down".into(),
        };
        assert_eq!(err.http_status(), Some(429));
        let err = Error::Llm {
            provider: "anthropic".into(),
            message: "HTTP 503 Service Unavailable: upstream".into(),
        };
        assert_eq!(err.http_status(), Some(503));
        assert_eq!(
            Error::RateLimited {
                provider: "x".into(),
                retry_after_ms: 1
            }
            .http_status(),
            Some(429)
        );
    }

    #[test]
    fn http_status_none_for_non_http_errors() {
        assert_eq!(Error::Timeout { duration_ms: 1 }.http_status(), None);
        assert_eq!(
            Error::Llm {
                provider: "x".into(),
                message: "request failed: connection reset".into(),
            }
            .http_status(),
            None
        );
        assert_eq!(
            Error::Llm {
                provider: "x".into(),
                message: "upstream 5xx".into(),
            }
            .http_status(),
            None,
            "a bare '5xx' token without the HTTP prefix must not parse"
        );
    }

    #[test]
    fn is_network_error_covers_transport_failures() {
        assert!(Error::Timeout { duration_ms: 1 }.is_network_error());
        assert!(Error::Llm {
            provider: "x".into(),
            message: "request failed: error sending request for url".into(),
        }
        .is_network_error());
        assert!(Error::Llm {
            provider: "x".into(),
            message: "SSE stream read error: connection closed".into(),
        }
        .is_network_error());
        assert!(!Error::Llm {
            provider: "x".into(),
            message: "invalid tool schema".into(),
        }
        .is_network_error());
    }

    #[tokio::test]
    async fn is_network_error_covers_http_transport_variant() {
        // Build a reqwest error without a network round-trip: an unusable URL
        // fails at request-build time.
        let e = reqwest::Client::new()
            .get("::not-a-url::")
            .send()
            .await
            .expect_err("invalid URL must fail request construction");
        assert!(Error::Http(e).is_network_error());
    }

    #[test]
    fn is_transient_provider_error_classifies_retryable_statuses() {
        let llm = |msg: &str| Error::Llm {
            provider: "x".into(),
            message: msg.to_string(),
        };
        assert!(llm("HTTP 429 Too Many Requests: x").is_transient_provider_error());
        assert!(llm("HTTP 500 Internal Server Error: x").is_transient_provider_error());
        assert!(llm("HTTP 502 Bad Gateway: x").is_transient_provider_error());
        assert!(llm("request failed: connection reset").is_transient_provider_error());
        // Permanent client errors must not be retried.
        assert!(!llm("HTTP 400 Bad Request: x").is_transient_provider_error());
        assert!(!llm("HTTP 401 Unauthorized: x").is_transient_provider_error());
        assert!(!llm("HTTP 200 but response body is empty").is_transient_provider_error());
        assert!(!Error::Config {
            message: "x".into()
        }
        .is_transient_provider_error());
        assert!(!Error::Tool {
            name: "x".into(),
            call_id: None,
            message: "x".into()
        }
        .is_transient_provider_error());
    }

    /// Issue #123: only a provider-side failure may count against `/readyz` —
    /// a request-caused 4xx (an oversized prompt earning a 400) says nothing
    /// about the endpoint, and every non-LLM variant is some other subsystem's
    /// fault.
    #[test]
    fn is_llm_failure_covers_only_provider_side_failures() {
        let llm = |message: &str| Error::Llm {
            provider: "openai".into(),
            message: message.into(),
        };
        for message in [
            "HTTP 401 Unauthorized: bad key",
            "HTTP 403 Forbidden: key lacks model access",
            "HTTP 429 Too Many Requests: slow down",
            "HTTP 503 Service Unavailable: upstream down",
            // A 2xx we could not use is still the provider's fault.
            "HTTP 200 but response body is empty",
            // No status: the endpoint never answered — a dropped send, a
            // stream read error, or a body we could not parse.
            "request failed: connection refused",
            "SSE stream read error: unexpected eof",
        ] {
            assert!(
                llm(message).is_llm_failure(),
                "{message} must count against readiness"
            );
        }
        for message in [
            // Request-caused: a healthy endpoint rejecting our payload.
            "HTTP 400 Bad Request: prompt is too long",
            "HTTP 404 Not Found: unknown model",
            "HTTP 422 Unprocessable Entity: bad tool schema",
        ] {
            assert!(
                !llm(message).is_llm_failure(),
                "{message} must not pull a healthy pod out of rotation"
            );
        }

        assert!(Error::RateLimited {
            provider: "openai".into(),
            retry_after_ms: 1000,
        }
        .is_llm_failure());
        assert!(!Error::Cancelled.is_llm_failure());
        assert!(!Error::WallClockExceeded { secs: 5 }.is_llm_failure());
        assert!(!Error::Tool {
            name: "Bash".into(),
            call_id: None,
            message: "boom".into(),
        }
        .is_llm_failure());
        assert!(!Error::Storage {
            message: "read-only filesystem".into(),
        }
        .is_llm_failure());
        assert!(!Error::ProviderTruncated("length".into()).is_llm_failure());
    }

    // ── is_context_window_exceeded ────────────────────────────────────

    #[test]
    fn context_overflow_matches_known_patterns() {
        let cases = [
            "HTTP 400: {\"error\":{\"code\":\"context_length_exceeded\",\"message\":\"too long\"}}",
            "HTTP 400: This model's maximum context length is 200000 tokens",
            "HTTP 400: prompt is too long for the model",
            "HTTP 400: tokens exceeds model limit",
            "HTTP 400: exceeds the model context window",
        ];
        for msg in &cases {
            let err = Error::Llm {
                provider: "test".into(),
                message: msg.to_string(),
            };
            assert!(
                is_context_window_exceeded(&err),
                "should detect context overflow in: {msg}"
            );
        }
    }

    #[test]
    fn context_overflow_ignores_unrelated_errors() {
        let cases = [
            "HTTP 400: invalid request body",
            "HTTP 401: unauthorized",
            "HTTP 429: rate limit exceeded",
            "network error: connection refused",
        ];
        for msg in &cases {
            let err = Error::Llm {
                provider: "test".into(),
                message: msg.to_string(),
            };
            assert!(
                !is_context_window_exceeded(&err),
                "should NOT detect context overflow in: {msg}"
            );
        }
    }

    #[test]
    fn context_overflow_ignores_non_llm_errors() {
        assert!(!is_context_window_exceeded(&Error::Timeout {
            duration_ms: 30_000
        }));
        let err = Error::Config {
            message: "context_length_exceeded".into(),
        };
        assert!(
            !is_context_window_exceeded(&err),
            "Config errors must not be detected even if they contain the keyword"
        );
    }
}
