//! Shared fixtures for the HTTP API integration tests.
//!
//! Extracted from the monolithic `tests/http.rs` during the P0-3 cleanup
//! so that fixture builders (mock config / app state) live next to their
//! consumers but are not intermingled with `#[test]` bodies. This makes
//! the file easier to read and primes the ground for a future per-feature-
//! area split of `tests/http.rs` — every consumer file will then
//! `#[allow(dead_code)] mod common;` to share the same fixtures.
//!
//! Note: this module sits under `tests/http_common/mod.rs` rather than
//! `tests/http_common.rs` so cargo does NOT treat it as an additional
//! integration test target. (Every `tests/*.rs` is its own test binary;
//! a top-level `http_common.rs` would compile to a "no tests" binary
//! and produce cargo warnings.) See
//! <https://doc.rust-lang.org/book/ch11-03-test-organization.html#submodules-in-integration-tests>
//! for the canonical pattern.

#![allow(dead_code)]

use recursive::config::Config;
use recursive::http::{AppState, Metrics, RateLimiter, ToolInfo};
use recursive::llm::{Completion, MockProvider};
use recursive::tools::ToolRegistry;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Since Goal 277, the HTTP server default-deny requires the
/// insecure-ok debug escape hatch for no-auth integration tests.
pub static SET_INSECURE_OK: std::sync::Once = std::sync::Once::new();

pub fn mock_config() -> Config {
    Config {
        workspace: PathBuf::from("/tmp"),
        api_base: "https://example.invalid/v1".into(),
        api_key: Some("test-key".into()),
        model: "mock".into(),
        provider_type: "openai".into(),
        preset: None,
        max_steps: 32,
        temperature: 0.0,
        system_prompt: "You are a test assistant.".into(),
        retry_max: 0,
        retry_initial_backoff_secs: 1,
        retry_max_backoff_secs: 1,
        shell_timeout_secs: 5,
        headless: false,
        memory_summary_limit: 5,
        thinking_budget: None,
        session_name: None,
        max_budget_usd: None,
        extra_dirs: Vec::new(),
        extra_readonly_dirs: Vec::new(),
        allow_tools: Vec::new(),
        context_window_override: None,
        subagent_max_depth: 2,
        subagent_enabled: false,
        allow_bypass_permissions: false,
        max_search_rounds: 3,
        stuck_window: 10,
        stuck_error_rate: 0.8,
        max_concurrent_runs: 8,
        goal_eval_transcript_tail: 12,
        web_search_provider: None,
        web_search_api_key: None,
        web_search_jina_key: None,
        wall_timeout_secs: 0,
    }
}

pub fn sample_state() -> AppState {
    SET_INSECURE_OK.call_once(|| {
        unsafe { std::env::set_var("RECURSIVE_HTTP_AUTH_INSECURE_OK", "1") };
    });
    let provider = Arc::new(MockProvider::new(vec![Completion {
        content: "hello".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));
    AppState {
        tools: vec![
            ToolInfo {
                name: "Read".into(),
                description: "Read a file from the workspace".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" }
                    },
                    "required": ["path"]
                }),
            },
            ToolInfo {
                name: "Write".into(),
                description: "Write content to a file".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" }
                    },
                    "required": ["path", "content"]
                }),
            },
        ],
        config: mock_config(),
        tool_registry: ToolRegistry::local(),
        provider,
        sessions: Arc::new(RwLock::new(HashMap::new())),
        event_channels: Arc::new(RwLock::new(HashMap::new())),
        metrics: Arc::new(Metrics::default()),
        slash_commands: Arc::new(Vec::new()),
        session_ttl_secs: 0,
        run_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(8)),
        rate_limiter: RateLimiter::new(10, 1.0),
        skills: vec![],
    }
}

pub fn sample_state_with_provider(provider: Arc<MockProvider>) -> AppState {
    SET_INSECURE_OK.call_once(|| {
        unsafe { std::env::set_var("RECURSIVE_HTTP_AUTH_INSECURE_OK", "1") };
    });
    AppState {
        tools: vec![],
        config: mock_config(),
        tool_registry: ToolRegistry::local(),
        provider,
        sessions: Arc::new(RwLock::new(HashMap::new())),
        event_channels: Arc::new(RwLock::new(HashMap::new())),
        metrics: Arc::new(Metrics::default()),
        slash_commands: Arc::new(Vec::new()),
        session_ttl_secs: 0,
        run_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(8)),
        rate_limiter: RateLimiter::new(10, 1.0),
        skills: vec![],
    }
}
