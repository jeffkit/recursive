//! Crate-wide error and Result.
//!
//! We keep the error surface small: anything internal collapses into
//! `Error::Other`, and external integrations (HTTP, JSON, IO) get dedicated
//! variants so callers can match if they want to recover.

use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("llm error: {0}")]
    Llm(String),

    #[error("tool `{name}` failed: {message}")]
    Tool { name: String, message: String },

    #[error("tool `{0}` not found")]
    UnknownTool(String),

    #[error("invalid tool arguments for `{name}`: {message}")]
    BadToolArgs { name: String, message: String },

    #[error("llm response truncated by provider (finish_reason = {0:?})")]
    ProviderTruncated(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("config: {0}")]
    Config(String),

    #[error("{0}")]
    Other(String),
}

impl From<anyhow::Error> for Error {
    fn from(value: anyhow::Error) -> Self {
        Error::Other(value.to_string())
    }
}
