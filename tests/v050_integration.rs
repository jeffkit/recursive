//! Integration tests for v0.5.0 cross-module features.
//!
//! Tests that HTTP API and Multi-Agent features work together correctly.

#[cfg(feature = "http")]
mod v050_integration {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use recursive::http::{build_router, AppState, Metrics, RateLimiter, ToolInfo};
    use recursive::llm::{mock::MockProvider, Completion};
    use recursive::multi::{default_roles, AgentPool};
    use recursive::tools::ToolRegistry;
    use recursive::{Config, LlmProvider};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    fn mock_provider() -> Arc<dyn LlmProvider> {
        Arc::new(MockProvider::new(vec![Completion {
            content: "I'll help you with that. Let me analyze the code.".to_string(),
            tool_calls: vec![],
            finish_reason: Some("stop".to_string()),
            usage: None,
            reasoning_content: None,
        }]))
    }

    fn test_config() -> Config {
        Config::from_env().unwrap_or_else(|_| Config {
            workspace: std::path::PathBuf::from("."),
            api_base: "http://localhost:11434/v1".into(),
            api_key: Some("test-key".into()),
            model: "test-model".into(),
            provider_type: "openai".into(),
            preset: None,
            max_steps: 10,
            temperature: 0.2,
            system_prompt: "You are a helpful assistant.".into(),
            retry_max: 0,
            retry_initial_backoff_secs: 1,
            retry_max_backoff_secs: 10,
            shell_timeout_secs: 30,
            headless: false,
            memory_summary_limit: 5,
            thinking_budget: None,
            session_name: None,
            max_budget_usd: None,
            extra_dirs: Vec::new(),
            allow_tools: Vec::new(),
            context_window_override: None,
            subagent_max_depth: 2,
            allow_bypass_permissions: false,
            max_search_rounds: 3,
            stuck_window: 10,
            stuck_error_rate: 0.8,
            max_concurrent_runs: 8,
            goal_eval_transcript_tail: 12,
        })
    }

    fn test_app_state() -> AppState {
        // Since Goal 277, the HTTP server default-deny requires the
        // insecure-ok escape hatch for no-auth integration tests.
        std::env::set_var("RECURSIVE_HTTP_AUTH_INSECURE_OK", "1");

        AppState {
            tools: vec![ToolInfo {
                name: "Read".into(),
                description: "Read a file".into(),
                parameters: serde_json::json!({}),
            }],
            config: test_config(),
            tool_registry: ToolRegistry::local(),
            provider: mock_provider(),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            event_channels: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(Metrics::default()),
            slash_commands: Arc::new(Vec::new()),
            session_ttl_secs: 0,
            run_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(8)),
            rate_limiter: RateLimiter::new(10, 1.0),
        }
    }

    // ── HTTP + Sessions Integration ──────────────────────────────────────

    #[tokio::test]
    async fn full_session_lifecycle() {
        let app = build_router(test_app_state());

        // Create session
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"system_prompt": "You are a helpful assistant"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let session: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let session_id = session["id"].as_str().unwrap();

        // List sessions
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let sessions: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(sessions.len(), 1);

        // Send message
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/sessions/{session_id}/messages"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"content": "hello"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        // Get session detail
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/sessions/{session_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        // Delete session
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri(format!("/sessions/{session_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 204);

        // Verify deleted
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/sessions/{session_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }

    // ── Multi-Agent Integration ──────────────────────────────────────────

    #[tokio::test]
    async fn multi_agent_pool_with_shared_memory() {
        let provider = mock_provider();
        let config = test_config();
        let mut pool = AgentPool::new(provider, config);

        // Register default roles
        for role in default_roles() {
            pool.add_role(role);
        }
        assert_eq!(pool.role_count(), 3);

        // Write to shared memory
        pool.memory()
            .set(
                "architecture".into(),
                "modular with clear boundaries".into(),
                "planner".into(),
            )
            .await;

        // Verify memory is accessible
        let entry = pool.memory().get("architecture").await.unwrap();
        assert_eq!(entry.value, "modular with clear boundaries");
        assert_eq!(entry.author, "planner");

        // Memory context includes the entry
        let ctx = pool.memory().to_context_string().await;
        assert!(ctx.contains("architecture"));
        assert!(ctx.contains("modular with clear boundaries"));
    }

    #[tokio::test]
    async fn multi_agent_messaging_bus() {
        let provider = mock_provider();
        let config = test_config();
        let pool = AgentPool::new(provider, config);

        // Send task from planner to coder
        pool.send_task("planner", "coder", "implement feature X")
            .await;
        pool.send_result("coder", "planner", "done, all tests pass")
            .await;

        // Check inboxes
        let coder_inbox = pool.bus().inbox("coder").await;
        assert_eq!(coder_inbox.len(), 1);
        assert_eq!(coder_inbox[0].content, "implement feature X");

        let planner_inbox = pool.bus().inbox("planner").await;
        assert_eq!(planner_inbox.len(), 1);
        assert_eq!(planner_inbox[0].content, "done, all tests pass");

        // Check history
        let history = pool.bus().history().await;
        assert_eq!(history.len(), 2);
    }

    // ── OpenAPI Spec Validation ──────────────────────────────────────────

    #[tokio::test]
    async fn openapi_spec_is_valid_and_complete() {
        let app = build_router(test_app_state());

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/openapi.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let spec: serde_json::Value = serde_json::from_slice(&body).unwrap();

        // Validate structure
        assert_eq!(spec["openapi"], "3.0.3");
        assert_eq!(spec["info"]["title"], "Recursive Agent API");
        assert_eq!(spec["info"]["version"], "0.4.0"); // Will need update in future

        // All endpoints documented
        let paths = spec["paths"].as_object().unwrap();
        assert!(paths.contains_key("/health"));
        assert!(paths.contains_key("/tools"));
        assert!(paths.contains_key("/run"));
        assert!(paths.contains_key("/sessions"));
        assert!(paths.contains_key("/openapi.json"));
    }
}
