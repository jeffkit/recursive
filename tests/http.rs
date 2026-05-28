//! Integration tests for the HTTP API (feature = "http").

#[cfg(feature = "http")]
mod http_tests {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use recursive::config::Config;
    use recursive::http::{
        build_router, build_router_with_auth, build_router_with_auth_and_rate_limit,
        map_agent_event, AppState, AuthConfig, Metrics, RateLimiter, SseEvent, ToolInfo,
    };
    use recursive::llm::{Completion, MockProvider};
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
            max_steps: 32,
            temperature: 0.0,
            system_prompt: "You are a test assistant.".into(),
            retry_max: 0,
            retry_initial_backoff_secs: 1,
            retry_max_backoff_secs: 1,
            shell_timeout_secs: 5,
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
}
