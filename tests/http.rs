//! Integration tests for the HTTP API (feature = "http").

#[cfg(feature = "http")]
mod http_tests {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use recursive::config::Config;
    use recursive::http::{
        build_router, build_router_with_auth, build_router_with_auth_and_rate_limit,
        map_agent_event, AppState, AuthConfig, JwtConfig, Metrics, RateLimiter, SessionState,
        SseEvent, ToolInfo,
    };
    use recursive::llm::{Completion, MockProvider};
    use recursive::runtime::AgentRuntimeBuilder;
    use recursive::tools::ToolRegistry;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::sync::{broadcast, RwLock};
    use tower::ServiceExt;

    fn mock_config() -> Config {
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
        }
    }

    fn sample_state() -> AppState {
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
                    name: "read_file".into(),
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
                    name: "write_file".into(),
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
        }
    }

    fn sample_state_with_provider(provider: Arc<MockProvider>) -> AppState {
        AppState {
            tools: vec![],
            config: mock_config(),
            tool_registry: ToolRegistry::local(),
            provider,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            event_channels: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(Metrics::default()),
            slash_commands: Arc::new(Vec::new()),
        }
    }

    #[tokio::test]
    async fn health_returns_ok() {
        let app = build_router(sample_state());

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"ok");
    }

    #[tokio::test]
    async fn tools_returns_json_array() {
        let app = build_router(sample_state());

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/tools")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let tools: Vec<ToolInfo> = serde_json::from_slice(&body).unwrap();
        assert_eq!(tools.len(), 2);
    }

    #[tokio::test]
    async fn tools_contains_expected_names() {
        let app = build_router(sample_state());

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/tools")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let tools: Vec<ToolInfo> = serde_json::from_slice(&body).unwrap();

        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"write_file"));
    }

    #[tokio::test]
    async fn tools_empty_state_returns_empty_array() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let app = build_router(AppState {
            tools: vec![],
            config: mock_config(),
            tool_registry: ToolRegistry::local(),
            provider,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            event_channels: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(Metrics::default()),
            slash_commands: Arc::new(Vec::new()),
        });

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/tools")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let tools: Vec<ToolInfo> = serde_json::from_slice(&body).unwrap();
        assert!(tools.is_empty());
    }

    // --- POST /run tests ---

    #[tokio::test]
    async fn run_with_mock_provider_returns_200() {
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "I completed the task.".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: Some(recursive::llm::TokenUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                cache_hit_tokens: 0,
                cache_miss_tokens: 0,
            }),
            reasoning_content: None,
        }]));

        let state = AppState {
            tools: vec![],
            config: mock_config(),
            tool_registry: ToolRegistry::local(),
            provider,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            event_channels: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(Metrics::default()),
            slash_commands: Arc::new(Vec::new()),
        };
        let app = build_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/run")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({
                            "goal": "Say hello"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let resp: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(resp["status"], "success");
        assert!(resp["finish_reason"]
            .as_str()
            .unwrap()
            .contains("NoMoreToolCalls"));
        assert!(resp["messages"].is_array());
        assert!(!resp["messages"].as_array().unwrap().is_empty());
        assert_eq!(resp["usage"]["total_steps"], 1);
        assert_eq!(resp["usage"]["total_tokens"], 15);
    }

    #[tokio::test]
    async fn run_with_missing_goal_returns_400() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let state = AppState {
            tools: vec![],
            config: mock_config(),
            tool_registry: ToolRegistry::local(),
            provider,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            event_channels: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(Metrics::default()),
            slash_commands: Arc::new(Vec::new()),
        };
        let app = build_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/run")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({
                            "goal": ""
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 400);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let resp: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(resp["status"], "error");
        assert!(resp["error"].as_str().unwrap().contains("goal"));
    }

    #[tokio::test]
    async fn run_response_has_expected_fields() {
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "Done.".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: Some(recursive::llm::TokenUsage {
                prompt_tokens: 20,
                completion_tokens: 10,
                total_tokens: 30,
                cache_hit_tokens: 0,
                cache_miss_tokens: 0,
            }),
            reasoning_content: None,
        }]));

        let state = AppState {
            tools: vec![],
            config: mock_config(),
            tool_registry: ToolRegistry::local(),
            provider,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            event_channels: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(Metrics::default()),
            slash_commands: Arc::new(Vec::new()),
        };
        let app = build_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/run")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({
                            "goal": "Test goal",
                            "max_steps": 5,
                            "system_prompt": "You are a terse assistant."
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let resp: serde_json::Value = serde_json::from_slice(&body).unwrap();

        // Verify all expected top-level fields exist
        assert!(resp.get("status").is_some(), "missing 'status' field");
        assert!(
            resp.get("finish_reason").is_some(),
            "missing 'finish_reason' field"
        );
        assert!(resp.get("messages").is_some(), "missing 'messages' field");
        assert!(resp.get("usage").is_some(), "missing 'usage' field");

        // Verify usage sub-fields
        let usage = &resp["usage"];
        assert!(
            usage.get("total_steps").is_some(),
            "missing 'usage.total_steps'"
        );
        assert!(
            usage.get("total_tokens").is_some(),
            "missing 'usage.total_tokens'"
        );

        // Verify values
        assert_eq!(resp["status"], "success");
        assert_eq!(usage["total_steps"], 1);
        assert_eq!(usage["total_tokens"], 30);
    }

    #[tokio::test]
    async fn run_with_no_goal_field_returns_422() {
        // Sending a body without the "goal" field at all should fail deserialization (422)
        let provider = Arc::new(MockProvider::new(vec![]));
        let state = AppState {
            tools: vec![],
            config: mock_config(),
            tool_registry: ToolRegistry::local(),
            provider,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            event_channels: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(Metrics::default()),
            slash_commands: Arc::new(Vec::new()),
        };
        let app = build_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/run")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({
                            "max_steps": 5
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        // axum returns 422 for deserialization failures
        assert_eq!(response.status(), 422);
    }

    #[tokio::test]
    async fn run_with_custom_max_steps_respected() {
        // Provider returns tool calls to exhaust a budget of 2 steps
        use recursive::llm::ToolCall;
        let provider = Arc::new(MockProvider::new(vec![
            Completion {
                content: "step 1".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "unknown".into(),
                    arguments: serde_json::json!({}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "step 2".into(),
                tool_calls: vec![ToolCall {
                    id: "c2".into(),
                    name: "unknown".into(),
                    arguments: serde_json::json!({}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "step 3".into(),
                tool_calls: vec![ToolCall {
                    id: "c3".into(),
                    name: "unknown".into(),
                    arguments: serde_json::json!({}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));

        let state = AppState {
            tools: vec![],
            config: mock_config(),
            tool_registry: ToolRegistry::local(),
            provider,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            event_channels: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(Metrics::default()),
            slash_commands: Arc::new(Vec::new()),
        };
        let app = build_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/run")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({
                            "goal": "loop forever",
                            "max_steps": 2
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let resp: serde_json::Value = serde_json::from_slice(&body).unwrap();

        // Should hit budget exceeded at 2 steps
        assert_eq!(resp["status"], "success");
        assert!(resp["finish_reason"]
            .as_str()
            .unwrap()
            .contains("BudgetExceeded"));
        assert_eq!(resp["usage"]["total_steps"], 2);
    }

    // ── Session endpoint tests ────────────────────────────────────────────

    #[tokio::test]
    async fn post_sessions_creates_session() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let state = sample_state_with_provider(provider);
        let app = build_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({})).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 201);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let resp: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert!(resp["id"].is_string());
        assert!(!resp["id"].as_str().unwrap().is_empty());
        assert!(resp["created_at"].is_string());
        assert!(resp["created_at"].as_str().unwrap().contains('T'));
    }

    #[tokio::test]
    async fn get_sessions_lists_sessions() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let state = sample_state_with_provider(provider);
        let app = build_router(state.clone());

        // Create a session first
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({})).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 201);

        // List sessions
        let app = build_router(state);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let resp: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();

        assert_eq!(resp.len(), 1);
        assert!(resp[0]["id"].is_string());
        assert_eq!(resp[0]["message_count"], 0);
    }

    #[tokio::test]
    async fn post_session_messages_returns_assistant_response() {
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "Hello! How can I help?".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let state = sample_state_with_provider(provider);

        // Create a session
        let app = build_router(state.clone());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({})).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let create_resp: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let session_id = create_resp["id"].as_str().unwrap();

        // Send a message
        let app = build_router(state);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/sessions/{}/messages", session_id))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({
                            "content": "Hi there"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let resp: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(resp["role"], "assistant");
        assert_eq!(resp["content"], "Hello! How can I help?");
    }

    #[tokio::test]
    async fn get_session_returns_session_with_messages() {
        // Create a provider with one response for when we send a message
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "I'm here to help.".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let state = sample_state_with_provider(provider);

        // Create a session
        let app = build_router(state.clone());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({
                            "system_prompt": "Be helpful."
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let create_resp: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let session_id = create_resp["id"].as_str().unwrap();

        // Send a message to populate the transcript
        let app = build_router(state.clone());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/sessions/{}/messages", session_id))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({
                            "content": "Hello"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);

        // Now GET the session detail
        let app = build_router(state);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/sessions/{}", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let resp: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(resp["id"], session_id);
        assert!(resp["created_at"].is_string());
        assert!(resp["messages"].is_array());
        // Should have at least system + user + assistant messages
        assert!(resp["messages"].as_array().unwrap().len() >= 3);
    }

    #[tokio::test]
    async fn delete_session_removes_it() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let state = sample_state_with_provider(provider);

        // Create a session
        let app = build_router(state.clone());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({})).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let create_resp: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let session_id = create_resp["id"].as_str().unwrap();

        // Delete it
        let app = build_router(state.clone());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri(format!("/sessions/{}", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 204);

        // Confirm it's gone
        let app = build_router(state);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/sessions/{}", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
    }

    #[tokio::test]
    async fn post_message_to_nonexistent_session_returns_404() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let state = sample_state_with_provider(provider);
        let app = build_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/sessions/nonexistent-id/messages")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({
                            "content": "Hello?"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 404);
    }

    // ── SSE endpoint tests ───────────────────────────────────────────────

    #[tokio::test]
    async fn session_events_returns_sse_content_type() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let state = sample_state_with_provider(provider);

        // Create a session first
        let app = build_router(state.clone());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({})).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let create_resp: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let session_id = create_resp["id"].as_str().unwrap();

        // Request SSE stream
        let app = build_router(state);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/sessions/{}/events", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let content_type = response
            .headers()
            .get("content-type")
            .expect("content-type header missing")
            .to_str()
            .unwrap();
        assert!(
            content_type.contains("text/event-stream"),
            "Expected text/event-stream, got: {}",
            content_type
        );
    }

    #[tokio::test]
    async fn session_events_nonexistent_returns_404() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let state = sample_state_with_provider(provider);
        let app = build_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/sessions/nonexistent-id/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 404);
    }

    #[tokio::test]
    async fn sse_event_serialization() {
        // Verify SseEvent serializes to expected JSON structure
        let tool_call = SseEvent::ToolCall {
            name: "read_file".into(),
            step: 1,
        };
        let json = serde_json::to_value(&tool_call).unwrap();
        assert_eq!(json["type"], "tool_call");
        assert_eq!(json["name"], "read_file");
        assert_eq!(json["step"], 1);

        let tool_result = SseEvent::ToolResult {
            name: "read_file".into(),
            success: true,
        };
        let json = serde_json::to_value(&tool_result).unwrap();
        assert_eq!(json["type"], "tool_result");
        assert_eq!(json["name"], "read_file");
        assert_eq!(json["success"], true);

        let done = SseEvent::Done {
            finish_reason: "NoMoreToolCalls".into(),
            total_steps: 3,
        };
        let json = serde_json::to_value(&done).unwrap();
        assert_eq!(json["type"], "done");
        assert_eq!(json["finish_reason"], "NoMoreToolCalls");
        assert_eq!(json["total_steps"], 3);

        let error = SseEvent::Error {
            message: "something went wrong".into(),
        };
        let json = serde_json::to_value(&error).unwrap();
        assert_eq!(json["type"], "error");
        assert_eq!(json["message"], "something went wrong");
    }

    #[tokio::test]
    async fn map_agent_event_tool_call() {
        use recursive::AgentEvent;

        let event = AgentEvent::ToolCall {
            name: "write_file".into(),
            id: "call_1".into(),
            arguments: r#"{"path": "/tmp/test"}"#.into(),
            step: 2,
        };
        let sse = map_agent_event(&event).unwrap();
        assert_eq!(
            sse,
            SseEvent::ToolCall {
                name: "write_file".into(),
                step: 2,
            }
        );
    }

    #[tokio::test]
    async fn map_agent_event_tool_result_success() {
        use recursive::AgentEvent;

        let event = AgentEvent::ToolResult {
            id: "call_1".into(),
            name: "read_file".into(),
            output: "file contents here".into(),
            step: 1,
        };
        let sse = map_agent_event(&event).unwrap();
        assert_eq!(
            sse,
            SseEvent::ToolResult {
                name: "read_file".into(),
                success: true,
            }
        );
    }

    #[tokio::test]
    async fn map_agent_event_tool_result_error() {
        use recursive::AgentEvent;

        let event = AgentEvent::ToolResult {
            id: "call_2".into(),
            name: "write_file".into(),
            output: "ERROR: permission denied".into(),
            step: 3,
        };
        let sse = map_agent_event(&event).unwrap();
        assert_eq!(
            sse,
            SseEvent::ToolResult {
                name: "write_file".into(),
                success: false,
            }
        );
    }

    #[tokio::test]
    async fn map_agent_event_turn_finished() {
        use recursive::AgentEvent;

        let event = AgentEvent::TurnFinished {
            reason: "NoMoreToolCalls".into(),
            steps: 5,
        };
        let sse = map_agent_event(&event).unwrap();
        assert_eq!(
            sse,
            SseEvent::Done {
                finish_reason: "NoMoreToolCalls".into(),
                total_steps: 5,
            }
        );
    }

    #[tokio::test]
    async fn map_agent_event_returns_none_for_unrelated() {
        use recursive::AgentEvent;

        let event = AgentEvent::Latency {
            step: 1,
            llm_ms: 500,
        };
        assert!(map_agent_event(&event).is_none());

        let event = AgentEvent::AssistantText {
            text: "hello".into(),
            step: 1,
        };
        assert!(map_agent_event(&event).is_none());
    }

    // ── New SDK-facing Message / PartialMessage events ───────────────────

    #[tokio::test]
    async fn map_message_appended_assistant_text_only() {
        use recursive::http::SseContentBlock;
        use recursive::message::{Message, Role};
        use recursive::AgentEvent;

        let event = AgentEvent::MessageAppended {
            message: Message {
                role: Role::Assistant,
                content: "Hi there".into(),
                tool_calls: vec![],
                tool_call_id: None,
                reasoning_content: None,
            },
            parent_uuid: None,
            usage: None,
        };
        let sse = map_agent_event(&event).unwrap();
        assert_eq!(
            sse,
            SseEvent::Message {
                role: "assistant".into(),
                content: vec![SseContentBlock::Text {
                    text: "Hi there".into(),
                }],
            }
        );
    }

    #[tokio::test]
    async fn map_message_appended_assistant_with_tool_calls() {
        use recursive::http::SseContentBlock;
        use recursive::llm::ToolCall;
        use recursive::message::{Message, Role};
        use recursive::AgentEvent;

        let event = AgentEvent::MessageAppended {
            message: Message {
                role: Role::Assistant,
                content: "calling".into(),
                tool_calls: vec![ToolCall {
                    id: "tc1".into(),
                    name: "read_file".into(),
                    arguments: serde_json::json!({"path": "x"}),
                }],
                tool_call_id: None,
                reasoning_content: None,
            },
            parent_uuid: None,
            usage: None,
        };
        let sse = map_agent_event(&event).unwrap();
        let SseEvent::Message { role, content } = sse else {
            panic!("expected Message variant");
        };
        assert_eq!(role, "assistant");
        assert_eq!(content.len(), 2);
        assert!(matches!(&content[0], SseContentBlock::Text { text } if text == "calling"));
        assert!(matches!(
            &content[1],
            SseContentBlock::ToolUse { id, name, .. } if id == "tc1" && name == "read_file"
        ));
    }

    #[tokio::test]
    async fn map_message_appended_skips_system_and_tool_roles() {
        use recursive::message::{Message, Role};
        use recursive::AgentEvent;

        for role in [Role::System, Role::Tool] {
            let event = AgentEvent::MessageAppended {
                message: Message {
                    role,
                    content: "x".into(),
                    tool_calls: vec![],
                    tool_call_id: Some("tc".into()),
                    reasoning_content: None,
                },
                parent_uuid: None,
                usage: None,
            };
            assert!(
                map_agent_event(&event).is_none(),
                "role {role:?} should not produce a Message event"
            );
        }
    }

    #[tokio::test]
    async fn map_partial_token_emits_partial_message() {
        use recursive::AgentEvent;

        let event = AgentEvent::PartialToken {
            text: "hel".into(),
            step: 3,
        };
        let sse = map_agent_event(&event).unwrap();
        assert_eq!(
            sse,
            SseEvent::PartialMessage {
                text: "hel".into(),
                step: 3,
            }
        );
    }

    #[tokio::test]
    async fn broadcast_channel_delivers_events() {
        // Verify that the broadcast channel properly delivers SseEvents
        let (tx, _) = broadcast::channel::<SseEvent>(64);
        let mut rx = tx.subscribe();

        let event = SseEvent::ToolCall {
            name: "test".into(),
            step: 1,
        };
        tx.send(event.clone()).unwrap();

        let received = rx.recv().await.unwrap();
        assert_eq!(received, event);
    }

    // ── OpenAPI spec tests ──────────────────────────────────────────────────

    #[tokio::test]
    async fn openapi_spec_returns_200() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let state = sample_state_with_provider(provider);
        let app = build_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/openapi.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
    }

    #[tokio::test]
    async fn openapi_spec_has_correct_version() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let state = sample_state_with_provider(provider);
        let app = build_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/openapi.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let spec: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(spec["openapi"], "3.0.3");
        assert_eq!(spec["info"]["title"], "Recursive Agent API");
        assert_eq!(spec["info"]["version"], "0.4.0");
    }

    #[tokio::test]
    async fn openapi_spec_has_all_paths() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let state = sample_state_with_provider(provider);
        let app = build_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/openapi.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let spec: serde_json::Value = serde_json::from_slice(&body).unwrap();

        let paths = spec["paths"]
            .as_object()
            .expect("paths should be an object");

        // All registered endpoints must be present
        assert!(paths.contains_key("/health"), "missing /health");
        assert!(paths.contains_key("/tools"), "missing /tools");
        assert!(paths.contains_key("/run"), "missing /run");
        assert!(paths.contains_key("/sessions"), "missing /sessions");
        assert!(
            paths.contains_key("/sessions/{id}"),
            "missing /sessions/{{id}}"
        );
        assert!(
            paths.contains_key("/sessions/{id}/messages"),
            "missing /sessions/{{id}}/messages"
        );
        assert!(
            paths.contains_key("/sessions/{id}/events"),
            "missing /sessions/{{id}}/events"
        );
        assert!(paths.contains_key("/openapi.json"), "missing /openapi.json");
    }

    // ------------------------------------------------------------------------
    // /metrics endpoint (Goal 134) — covers the Prometheus exposition format,
    // the auto-incrementing middleware, and the round-trip from atomic store
    // back into the rendered response body. Counter implementation lives in
    // src/http.rs (Goal 122 / commit 01792b7).
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn metrics_returns_prometheus_format() {
        let app = build_router(sample_state());

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = std::str::from_utf8(&body).unwrap();

        // Must contain HELP/TYPE preambles for at least one counter and one gauge.
        assert!(text.contains("# HELP recursive_requests_total"));
        assert!(text.contains("# TYPE recursive_requests_total counter"));
        assert!(text.contains("# TYPE recursive_requests_active gauge"));

        // Must list every metric name from the Metrics struct.
        for name in [
            "recursive_requests_total",
            "recursive_requests_active",
            "recursive_agent_runs_total",
            "recursive_agent_runs_success",
            "recursive_agent_runs_failed",
            "recursive_tokens_prompt_total",
            "recursive_tokens_completion_total",
            "recursive_agent_steps_total",
        ] {
            assert!(text.contains(name), "missing metric: {name}");
        }
    }

    #[tokio::test]
    async fn metrics_middleware_increments_requests_total() {
        let state = sample_state();
        let metrics = state.metrics.clone();
        let app = build_router(state);

        // Hit two non-/metrics endpoints to drive the middleware.
        for uri in ["/health", "/tools"] {
            let _ = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
        }

        let n = metrics
            .requests_total
            .load(std::sync::atomic::Ordering::Relaxed);
        assert!(n >= 2, "expected requests_total >= 2, got {n}");
    }

    #[tokio::test]
    async fn metrics_counter_values_render() {
        let state = sample_state();
        state
            .metrics
            .agent_runs_total
            .store(7, std::sync::atomic::Ordering::Relaxed);
        state
            .metrics
            .tokens_prompt_total
            .store(12345, std::sync::atomic::Ordering::Relaxed);
        let app = build_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains("recursive_agent_runs_total 7"));
        assert!(text.contains("recursive_tokens_prompt_total 12345"));
    }

    // ------------------------------------------------------------------------
    // Auth middleware (Goal 135) — API key authentication via X-API-Key.
    // Tests use build_router_with_auth() to inject a deterministic AuthConfig
    // and avoid env-var races (parallel cargo test threads share process env).
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn auth_disabled_passes_through() {
        // AuthConfig::default() == empty key set == auth disabled.
        let app = build_router_with_auth(sample_state(), AuthConfig::default());

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/tools")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
    }

    #[tokio::test]
    async fn auth_enabled_rejects_missing_header() {
        let app = build_router_with_auth(sample_state(), AuthConfig::new(vec!["secret".into()]));

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/tools")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 401);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"unauthorized");
    }

    #[tokio::test]
    async fn auth_enabled_accepts_valid_key() {
        let app = build_router_with_auth(sample_state(), AuthConfig::new(vec!["secret".into()]));

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/tools")
                    .header("X-API-Key", "secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
    }

    #[tokio::test]
    async fn auth_enabled_rejects_wrong_key() {
        let app = build_router_with_auth(sample_state(), AuthConfig::new(vec!["secret".into()]));

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/tools")
                    .header("X-API-Key", "bogus")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 401);
    }

    #[tokio::test]
    async fn auth_health_and_metrics_are_exempt() {
        // Even with auth enabled, /health and /metrics must answer
        // unauthenticated (k8s liveness + Prometheus scraping).
        let auth = AuthConfig::new(vec!["secret".into()]);
        let app = build_router_with_auth(sample_state(), auth);

        for uri in ["/health", "/metrics"] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                200,
                "expected {uri} to be exempt from auth"
            );
        }
    }

    #[tokio::test]
    async fn auth_config_is_valid_unit() {
        // Empty config: any input (including empty string) returns true.
        let empty = AuthConfig::default();
        assert!(empty.is_valid(""));
        assert!(empty.is_valid("anything"));
        assert!(!empty.is_enabled());

        // Populated config:
        let cfg = AuthConfig::new(vec!["alpha".into(), "beta".into()]);
        assert!(cfg.is_enabled());
        assert!(cfg.is_valid("alpha"));
        assert!(cfg.is_valid("beta"));
        assert!(!cfg.is_valid("alphA")); // wrong case
        assert!(!cfg.is_valid("alph")); // length-1 short
        assert!(!cfg.is_valid("alphax")); // length+1 long
        assert!(!cfg.is_valid("")); // empty rejected
        assert!(!cfg.is_valid("gamma")); // unrelated
    }

    // ------------------------------------------------------------------------
    // Rate limiter (Goal 139) — token-bucket integration tests through the
    // axum middleware stack. Uses build_router_with_auth_and_rate_limit to
    // inject a deterministic limiter without env-var races. Lower-level
    // unit tests of RateLimiter::check / extract_client_key live inside
    // src/http.rs and are not duplicated here.
    // ------------------------------------------------------------------------

    fn router_with_limiter(limiter: RateLimiter) -> axum::Router {
        build_router_with_auth_and_rate_limit(sample_state(), AuthConfig::default(), limiter)
    }

    #[tokio::test]
    async fn rate_limit_first_request_succeeds() {
        // capacity=2 with very slow refill; first hit should always pass.
        let app = router_with_limiter(RateLimiter::new(2, 0.001));

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
    }

    #[tokio::test]
    async fn rate_limit_burst_allowed_then_429() {
        // capacity=2: first 2 requests within burst → 200; third → 429.
        // Refill rate is tiny (0.001/s ≈ 1 token per 16 minutes) so the
        // bucket cannot replenish during test runtime.
        let app = router_with_limiter(RateLimiter::new(2, 0.001));

        for i in 0..2 {
            let resp = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri("/tools")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), 200, "request #{} should succeed", i + 1);
        }

        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/tools")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 429, "third request should be rate-limited");
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"rate limit exceeded");
    }

    #[tokio::test]
    async fn rate_limit_different_clients_have_independent_buckets() {
        // capacity=1 per client. Two clients distinguished by X-API-Key.
        // Client A's second hit should be 429 (its bucket is exhausted),
        // while Client B's first hit is still 200 (its bucket is full).
        let app = router_with_limiter(RateLimiter::new(1, 0.001));

        let req = |key: &'static str| {
            let app = app.clone();
            async move {
                app.oneshot(
                    axum::http::Request::builder()
                        .uri("/tools")
                        .header("X-API-Key", key)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
                .status()
            }
        };

        assert_eq!(req("alpha").await, 200, "alpha first hit");
        assert_eq!(req("beta").await, 200, "beta first hit");
        assert_eq!(req("alpha").await, 429, "alpha second hit (exhausted)");
    }

    #[tokio::test]
    async fn rate_limit_does_not_block_below_threshold() {
        // High capacity: 5 sequential hits should all pass.
        let app = router_with_limiter(RateLimiter::new(100, 0.001));

        for i in 0..5 {
            let resp = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri("/tools")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), 200, "request #{} should pass", i + 1);
        }
    }

    // ------------------------------------------------------------------------
    // JWT bearer token auth (Goal 136). Verify-only — these tests mint
    // tokens at runtime using the same jsonwebtoken crate the server
    // uses to verify them. AuthConfig::with_jwt attaches a JwtConfig
    // alongside (or instead of) API keys; auth_middleware accepts
    // either credential type.
    // ------------------------------------------------------------------------

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn mint_token(secret: &str, exp_offset_secs: i64, audience: Option<&str>) -> String {
        use jsonwebtoken::{encode, EncodingKey, Header};
        let exp = (now_secs() as i64) + exp_offset_secs;
        let mut claims = serde_json::json!({ "exp": exp, "sub": "test-user" });
        if let Some(aud) = audience {
            claims["aud"] = serde_json::Value::String(aud.into());
        }
        encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .expect("mint jwt")
    }

    fn router_with_jwt_only(secret: &str, audience: Option<&str>) -> axum::Router {
        let jwt = JwtConfig::hs256(secret, audience.map(|s| s.to_string())).unwrap();
        let auth = AuthConfig::new(Vec::new()).with_jwt(jwt);
        build_router_with_auth(sample_state(), auth)
    }

    #[tokio::test]
    async fn jwt_disabled_legacy_keys_only() {
        // No JWT verifier configured — bearer header is meaningless;
        // only X-API-Key works.
        let auth = AuthConfig::new(vec!["legacy-key".into()]);
        let app = build_router_with_auth(sample_state(), auth);

        // Bearer token in header is rejected — JWT not enabled.
        let token = mint_token("any-secret", 60, None);
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/tools")
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 401);

        // X-API-Key still works.
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/tools")
                    .header("X-API-Key", "legacy-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
    }

    #[tokio::test]
    async fn jwt_valid_token_accepted() {
        let secret = "test-secret-12345";
        let app = router_with_jwt_only(secret, None);

        let token = mint_token(secret, 60, None);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/tools")
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
    }

    #[tokio::test]
    async fn jwt_expired_token_rejected() {
        let secret = "test-secret-12345";
        let app = router_with_jwt_only(secret, None);

        // 5 minutes in the past — well outside jsonwebtoken's default
        // 60-second clock-skew leeway.
        let token = mint_token(secret, -300, None);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/tools")
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
    }

    #[tokio::test]
    async fn jwt_wrong_signature_rejected() {
        let app = router_with_jwt_only("server-secret", None);
        // Token minted with a different secret
        let token = mint_token("attacker-secret", 60, None);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/tools")
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
    }

    #[tokio::test]
    async fn jwt_audience_mismatch_rejected() {
        let secret = "test-secret-12345";
        let app = router_with_jwt_only(secret, Some("expected-aud"));
        // Token has aud="other"
        let token = mint_token(secret, 60, Some("other-aud"));
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/tools")
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
    }

    #[tokio::test]
    async fn jwt_audience_match_accepted() {
        let secret = "test-secret-12345";
        let app = router_with_jwt_only(secret, Some("expected-aud"));
        let token = mint_token(secret, 60, Some("expected-aud"));
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/tools")
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
    }

    #[tokio::test]
    async fn jwt_or_api_key_either_works() {
        let secret = "test-secret-12345";
        let jwt = JwtConfig::hs256(secret, None).unwrap();
        let auth = AuthConfig::new(vec!["legacy-key".into()]).with_jwt(jwt);
        let app = build_router_with_auth(sample_state(), auth);

        // No credentials → 401
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/tools")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 401);

        // Valid X-API-Key → 200
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/tools")
                    .header("X-API-Key", "legacy-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);

        // Valid JWT → 200
        let token = mint_token(secret, 60, None);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/tools")
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
    }

    #[tokio::test]
    async fn jwt_health_metrics_remain_exempt() {
        let app = router_with_jwt_only("test-secret-12345", None);

        for uri in ["/health", "/metrics"] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                200,
                "{uri} should be exempt from JWT auth"
            );
        }
    }

    // ── /agui endpoint tests ──────────────────────────────────────────────

    /// Drain an SSE response body into a Vec<agui_protocol::Event> by
    /// feeding all body bytes through the protocol's SseParser.
    async fn collect_agui_events(
        response: axum::http::Response<Body>,
    ) -> Vec<agui_protocol::Event> {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let mut parser = agui_protocol::SseParser::new();
        parser.feed(&bytes)
    }

    fn agui_request_body(messages: serde_json::Value, context: serde_json::Value) -> String {
        serde_json::to_string(&serde_json::json!({
            "threadId": "t-test",
            "runId": "r-test",
            "messages": messages,
            "tools": [],
            "context": context,
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn agui_endpoint_streams_run_started_and_finished() {
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "hello from mock".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let state = sample_state_with_provider(provider);
        let app = build_router(state);

        let body = agui_request_body(
            serde_json::json!([
                {"id": "u1", "role": "user", "content": "say hello"}
            ]),
            serde_json::json!([]),
        );

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/agui")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let content_type = response
            .headers()
            .get("content-type")
            .expect("content-type header missing")
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            content_type.contains("text/event-stream"),
            "Expected text/event-stream, got: {content_type}",
        );

        let events = collect_agui_events(response).await;
        assert!(!events.is_empty(), "expected at least one AG-UI event");

        // First event must be RunStarted with the supplied ids.
        match &events[0] {
            agui_protocol::Event::RunStarted(rs) => {
                assert_eq!(rs.thread_id, "t-test");
                assert_eq!(rs.run_id, "r-test");
            }
            other => panic!("expected RunStarted first, got {other:?}"),
        }

        // Last event must be RunFinished.
        match events.last().unwrap() {
            agui_protocol::Event::RunFinished(rf) => {
                assert_eq!(rf.thread_id, "t-test");
                assert_eq!(rf.run_id, "r-test");
            }
            other => panic!("expected RunFinished last, got {other:?}"),
        }

        // Between them, we must see Start/Content/End for the assistant
        // text and they must be in that order.
        let positions: Vec<usize> = events
            .iter()
            .enumerate()
            .filter_map(|(i, e)| match e {
                agui_protocol::Event::TextMessageStart(_)
                | agui_protocol::Event::TextMessageContent(_)
                | agui_protocol::Event::TextMessageEnd(_) => Some(i),
                _ => None,
            })
            .collect();
        assert!(
            positions.len() >= 3,
            "expected Start/Content/End, got events: {events:?}"
        );
        match &events[positions[0]] {
            agui_protocol::Event::TextMessageStart(_) => {}
            other => panic!("expected TextMessageStart first, got {other:?}"),
        }
        let mut saw_content = false;
        for &i in &positions[1..positions.len() - 1] {
            if let agui_protocol::Event::TextMessageContent(c) = &events[i] {
                if c.delta.contains("hello from mock") {
                    saw_content = true;
                }
            }
        }
        assert!(
            saw_content,
            "expected a TextMessageContent carrying the assistant text"
        );
        match &events[*positions.last().unwrap()] {
            agui_protocol::Event::TextMessageEnd(_) => {}
            other => panic!("expected TextMessageEnd last, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn agui_endpoint_rejects_empty_messages_and_context() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let state = sample_state_with_provider(provider);
        let app = build_router(state);

        let body = agui_request_body(serde_json::json!([]), serde_json::json!([]));

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/agui")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 400);
    }

    #[tokio::test]
    async fn agui_endpoint_emits_tool_call_events() {
        use recursive::llm::ToolCall;
        let provider = Arc::new(MockProvider::new(vec![
            Completion {
                content: "calling a tool".into(),
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "unknown_tool".into(),
                    arguments: serde_json::json!({"foo": "bar"}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "all done".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));
        let state = sample_state_with_provider(provider);
        let app = build_router(state);

        let body = agui_request_body(
            serde_json::json!([
                {"id": "u1", "role": "user", "content": "go"}
            ]),
            serde_json::json!([]),
        );

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/agui")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let events = collect_agui_events(response).await;

        // Locate the four tool events and RunFinished.
        let mut idx_start = None;
        let mut idx_args = None;
        let mut idx_end = None;
        let mut idx_result = None;
        let mut idx_finished = None;
        for (i, ev) in events.iter().enumerate() {
            match ev {
                agui_protocol::Event::ToolCallStart(s) if s.tool_call_id == "call-1" => {
                    idx_start.get_or_insert(i);
                }
                agui_protocol::Event::ToolCallArgs(a) if a.tool_call_id == "call-1" => {
                    idx_args.get_or_insert(i);
                }
                agui_protocol::Event::ToolCallEnd(e) if e.tool_call_id == "call-1" => {
                    idx_end.get_or_insert(i);
                }
                agui_protocol::Event::ToolCallResult(r) if r.tool_call_id == "call-1" => {
                    idx_result.get_or_insert(i);
                }
                agui_protocol::Event::RunFinished(_) => {
                    idx_finished.get_or_insert(i);
                }
                _ => {}
            }
        }

        let s = idx_start.expect("missing ToolCallStart");
        let a = idx_args.expect("missing ToolCallArgs");
        let e = idx_end.expect("missing ToolCallEnd");
        let r = idx_result.expect("missing ToolCallResult");
        let f = idx_finished.expect("missing RunFinished");

        assert!(
            s < a && a < e && e < r && r < f,
            "tool events out of order: start={s} args={a} end={e} result={r} finished={f}; events={events:?}"
        );

        // The args delta should contain the JSON arguments.
        if let agui_protocol::Event::ToolCallArgs(args) = &events[a] {
            assert!(
                args.delta.contains("foo") && args.delta.contains("bar"),
                "args delta missing arguments: {}",
                args.delta
            );
        }
    }

    // ── Plan-mode HTTP endpoint tests ─────────────────────────────────────

    /// Helper: build a state that has one pre-inserted session whose gate has
    /// a pending plan (i.e. status == "plan_pending_approval").
    async fn state_with_pending_plan_session(plan_text: &str) -> (AppState, String) {
        let provider = Arc::new(MockProvider::new(vec![]));
        let state = sample_state_with_provider(provider);

        let runtime = AgentRuntimeBuilder::new()
            .llm(Arc::new(MockProvider::new(vec![])))
            .build()
            .expect("runtime build failed");
        let gate = runtime.plan_approval_gate();

        // Simulate the agent having set a pending plan.
        gate.pending_plan
            .write()
            .expect("write lock")
            .replace(plan_text.to_string());

        let session_id = "test-session-plan".to_string();
        let session = SessionState {
            id: session_id.clone(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            title: None,
            runtime: Arc::new(tokio::sync::Mutex::new(runtime)),
            plan_approval_gate: gate,
            interrupt_token: Arc::new(tokio::sync::Mutex::new(None)),
        };
        state
            .sessions
            .write()
            .await
            .insert(session_id.clone(), session);

        (state, session_id)
    }

    #[tokio::test]
    async fn plan_confirm_returns_404_for_unknown_session() {
        let app = build_router(sample_state());

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/sessions/nonexistent/plan/confirm")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn plan_reject_returns_404_for_unknown_session() {
        let app = build_router(sample_state());

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/sessions/nonexistent/plan/reject")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"reason":"no such session"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn plan_confirm_returns_409_when_no_plan_pending() {
        // Create a real session with no pending plan.
        let provider = Arc::new(MockProvider::new(vec![]));
        let state = sample_state_with_provider(provider);
        let app = build_router(state.clone());

        // Create session via API.
        let create_resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(create_resp.status(), 201);
        let body = create_resp.into_body().collect().await.unwrap().to_bytes();
        let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let session_id = created["id"].as_str().unwrap().to_string();

        // Confirm when no plan is pending → 409.
        let app2 = build_router(state);
        let confirm_resp = app2
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/sessions/{session_id}/plan/confirm"))
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(confirm_resp.status(), 409);
    }

    #[tokio::test]
    async fn plan_reject_returns_409_when_no_plan_pending() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let state = sample_state_with_provider(provider);
        let app = build_router(state.clone());

        let create_resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(create_resp.status(), 201);
        let body = create_resp.into_body().collect().await.unwrap().to_bytes();
        let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let session_id = created["id"].as_str().unwrap().to_string();

        let app2 = build_router(state);
        let reject_resp = app2
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/sessions/{session_id}/plan/reject"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"reason":"test"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(reject_resp.status(), 409);
    }

    #[tokio::test]
    async fn get_session_returns_plan_pending_approval_status() {
        let (state, session_id) =
            state_with_pending_plan_session("Step 1: read files\nStep 2: write summary").await;
        let app = build_router(state);

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/sessions/{session_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let detail: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(detail["status"], "plan_pending_approval");
        assert_eq!(
            detail["pending_plan"],
            "Step 1: read files\nStep 2: write summary"
        );
    }

    #[tokio::test]
    async fn plan_confirm_approves_pending_plan_and_returns_200() {
        let (state, session_id) = state_with_pending_plan_session("Do the thing").await;
        let app = build_router(state);

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/sessions/{session_id}/plan/confirm"))
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let result: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(result["status"], "approved");
        assert_eq!(result["session_id"], session_id.as_str());
    }

    #[tokio::test]
    async fn plan_confirm_with_edits_updates_plan_text() {
        let (state, session_id) = state_with_pending_plan_session("Original plan").await;

        // Snapshot the gate so we can verify the edit took effect.
        let gate = {
            state
                .sessions
                .read()
                .await
                .get(&session_id)
                .unwrap()
                .plan_approval_gate
                .clone()
        };

        let app = build_router(state);
        let body = serde_json::json!({"edits": "Revised plan"}).to_string();

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/sessions/{session_id}/plan/confirm"))
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);
        // The gate's pending_plan was updated before approve() was called.
        // After approve() the gate clears the response (not the pending_plan)
        // so the edited text is still readable.
        let stored = gate.pending_plan.read().unwrap().clone();
        assert_eq!(stored.as_deref(), Some("Revised plan"));
    }

    #[tokio::test]
    async fn plan_reject_rejects_pending_plan_and_returns_200() {
        let (state, session_id) = state_with_pending_plan_session("Plan to reject").await;
        let app = build_router(state);

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/sessions/{session_id}/plan/reject"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"reason":"not detailed enough"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let result: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(result["status"], "rejected");
        assert_eq!(result["session_id"], session_id.as_str());
    }

    #[tokio::test]
    async fn map_agent_event_plan_proposed_maps_to_sse() {
        use recursive::event::AgentEvent;
        let event = AgentEvent::PlanProposed {
            plan_text: "my plan".to_string(),
            tool_calls: vec![],
        };
        let sse = map_agent_event(&event);
        assert_eq!(
            sse,
            Some(SseEvent::PlanProposed {
                plan: "my plan".to_string()
            })
        );
    }

    #[tokio::test]
    async fn get_session_returns_idle_status_when_no_plan_pending() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let state = sample_state_with_provider(provider);
        let app = build_router(state.clone());

        let create_resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = create_resp.into_body().collect().await.unwrap().to_bytes();
        let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let session_id = created["id"].as_str().unwrap().to_string();

        let app2 = build_router(state);
        let detail_resp = app2
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/sessions/{session_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(detail_resp.status(), 200);
        let body = detail_resp.into_body().collect().await.unwrap().to_bytes();
        let detail: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(detail["status"], "idle");
        assert!(detail["pending_plan"].is_null());
    }

    // ── Goal-168: /goal endpoint tests ──────────────────────────────────────

    #[tokio::test]
    async fn set_goal_returns_200_for_valid_session() {
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "YES\nCondition met.".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let state = sample_state_with_provider(provider);
        let app = build_router(state.clone());

        // Create a session first.
        let create_resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/sessions")
                    .method("POST")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"system_prompt": null}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = create_resp.into_body().collect().await.unwrap().to_bytes();
        let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let session_id = created["id"].as_str().unwrap().to_string();

        // Set a goal.
        let app2 = build_router(state);
        let resp = app2
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/sessions/{session_id}/goal"))
                    .method("POST")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"condition": "Write a greeting", "max_turns": 3}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(val["status"], "pursuing");
        assert_eq!(val["session_id"], session_id);
    }

    #[tokio::test]
    async fn set_goal_returns_404_for_missing_session() {
        let app = build_router(sample_state());
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/sessions/no-such-session/goal")
                    .method("POST")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"condition": "anything"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn clear_goal_returns_200_for_valid_session() {
        let state = sample_state();
        let app = build_router(state.clone());

        // Create session.
        let create_resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/sessions")
                    .method("POST")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = create_resp.into_body().collect().await.unwrap().to_bytes();
        let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let session_id = created["id"].as_str().unwrap().to_string();

        // Delete goal (even though none is set — should still be 200).
        let app2 = build_router(state);
        let resp = app2
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/sessions/{session_id}/goal"))
                    .method("DELETE")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(val["status"], "cleared");
    }

    #[tokio::test]
    async fn clear_goal_returns_404_for_missing_session() {
        let app = build_router(sample_state());
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/sessions/ghost/goal")
                    .method("DELETE")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn session_detail_includes_goal_field_when_null() {
        let state = sample_state();
        let app = build_router(state.clone());

        let create_resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/sessions")
                    .method("POST")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = create_resp.into_body().collect().await.unwrap().to_bytes();
        let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let session_id = created["id"].as_str().unwrap().to_string();

        let app2 = build_router(state);
        let resp = app2
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/sessions/{session_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let detail: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // goal field should be present (as null) when no goal is set.
        assert!(detail.get("goal").is_some());
        assert!(detail["goal"].is_null());
    }

    // ── Goal-168 (extra): additional goal endpoint tests ─────────────────────

    #[tokio::test]
    async fn set_goal_response_includes_session_id_field() {
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "NO\nNot done yet.".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let state = sample_state_with_provider(provider);
        let app = build_router(state.clone());

        let create_resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/sessions")
                    .method("POST")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = create_resp.into_body().collect().await.unwrap().to_bytes();
        let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let session_id = created["id"].as_str().unwrap().to_string();

        let app2 = build_router(state);
        let resp = app2
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/sessions/{session_id}/goal"))
                    .method("POST")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"condition": "tests pass"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // Response must include both required fields.
        assert!(val.get("session_id").is_some());
        assert_eq!(val["session_id"], session_id);
        assert_eq!(val["status"], "pursuing");
    }

    #[tokio::test]
    async fn clear_goal_response_includes_session_id_field() {
        let state = sample_state();
        let app = build_router(state.clone());

        let create_resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/sessions")
                    .method("POST")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = create_resp.into_body().collect().await.unwrap().to_bytes();
        let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let session_id = created["id"].as_str().unwrap().to_string();

        let app2 = build_router(state);
        let resp = app2
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/sessions/{session_id}/goal"))
                    .method("DELETE")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(val.get("session_id").is_some());
        assert_eq!(val["session_id"], session_id);
        assert_eq!(val["status"], "cleared");
    }

    #[tokio::test]
    async fn set_goal_uses_default_max_turns_when_omitted() {
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "NO\nNot yet.".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let state = sample_state_with_provider(provider);
        let app = build_router(state.clone());

        let create_resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/sessions")
                    .method("POST")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = create_resp.into_body().collect().await.unwrap().to_bytes();
        let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let session_id = created["id"].as_str().unwrap().to_string();

        // Omit max_turns — server should default to 20.
        let app2 = build_router(state);
        let resp = app2
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/sessions/{session_id}/goal"))
                    .method("POST")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"condition": "all tests pass"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(val["status"], "pursuing");
    }

    // ── Goal-170: interrupt endpoint tests ────────────────────────────────────

    #[tokio::test]
    async fn interrupt_returns_200_for_valid_session() {
        let state = sample_state();
        let app = build_router(state.clone());

        let create_resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/sessions")
                    .method("POST")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = create_resp.into_body().collect().await.unwrap().to_bytes();
        let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let session_id = created["id"].as_str().unwrap().to_string();

        // Interrupt with no active run — should still be 200 (idempotent).
        let app2 = build_router(state);
        let resp = app2
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/sessions/{session_id}/interrupt"))
                    .method("POST")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(val["status"], "interrupted");
        assert_eq!(val["session_id"], session_id);
    }

    #[tokio::test]
    async fn interrupt_returns_404_for_missing_session() {
        let app = build_router(sample_state());
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/sessions/ghost/interrupt")
                    .method("POST")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }

    // ── Goal-169: /slash-commands endpoint tests ─────────────────────────────

    #[tokio::test]
    async fn slash_commands_returns_empty_list_when_none_configured() {
        let state = sample_state();
        let app = build_router(state);
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/slash-commands")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let cmds: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(cmds.is_array());
        // Our sample state has slash_commands: Arc::new(Vec::new())
        assert_eq!(cmds.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn slash_commands_returns_configured_commands() {
        use recursive::http::SlashCommandInfo;
        let provider = Arc::new(MockProvider::new(vec![]));
        let state = AppState {
            tools: vec![],
            config: mock_config(),
            tool_registry: ToolRegistry::local(),
            provider,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            event_channels: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(Metrics::default()),
            slash_commands: Arc::new(vec![
                SlashCommandInfo {
                    name: "deploy".to_string(),
                    description: "Deploy the service".to_string(),
                    source: "skill".to_string(),
                    aliases: vec!["d".to_string()],
                    argument_hint: "<env>".to_string(),
                },
                SlashCommandInfo {
                    name: "rollback".to_string(),
                    description: "Roll back the deployment".to_string(),
                    source: "skill".to_string(),
                    aliases: vec![],
                    argument_hint: String::new(),
                },
            ]),
        };
        let app = build_router(state);
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/slash-commands")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let cmds: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let arr = cmds.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["name"], "deploy");
        assert_eq!(arr[0]["source"], "skill");
        assert_eq!(arr[0]["aliases"][0], "d");
        assert_eq!(arr[0]["argument_hint"], "<env>");
        assert_eq!(arr[1]["name"], "rollback");
    }

    // ── fork_session ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn fork_session_returns_201_with_new_id() {
        let state = sample_state();
        let app = build_router(state.clone());

        // Create the source session.
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let src: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let src_id = src["id"].as_str().unwrap().to_string();

        // Fork it.
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/sessions/{src_id}/fork"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let fork: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let fork_id = fork["id"].as_str().unwrap();
        assert!(!fork_id.is_empty(), "fork id should be non-empty");
        assert_ne!(fork_id, src_id, "fork id must differ from source");
        // message_count reflects the source transcript at fork time (may include system init)
        assert!(fork["message_count"].as_u64().is_some());
    }

    #[tokio::test]
    async fn fork_session_returns_404_for_missing_session() {
        let state = sample_state();
        let app = build_router(state);

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/sessions/nonexistent/fork")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }
}
