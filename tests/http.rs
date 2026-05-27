//! Integration tests for the HTTP API (feature = "http").

#[cfg(feature = "http")]
mod http_tests {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use recursive::config::Config;
    use recursive::http::{build_router, map_step_event, AppState, Metrics, SseEvent, ToolInfo};
    use recursive::llm::{Completion, MockProvider};
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
    async fn map_step_event_tool_call() {
        use recursive::llm::ToolCall as LlmToolCall;
        use recursive::StepEvent;

        let step_event = StepEvent::ToolCall {
            call: LlmToolCall {
                id: "call_1".into(),
                name: "write_file".into(),
                arguments: serde_json::json!({"path": "/tmp/test"}),
            },
            step: 2,
        };
        let sse = map_step_event(&step_event).unwrap();
        assert_eq!(
            sse,
            SseEvent::ToolCall {
                name: "write_file".into(),
                step: 2,
            }
        );
    }

    #[tokio::test]
    async fn map_step_event_tool_result_success() {
        use recursive::StepEvent;

        let step_event = StepEvent::ToolResult {
            id: "call_1".into(),
            name: "read_file".into(),
            output: "file contents here".into(),
            step: 1,
        };
        let sse = map_step_event(&step_event).unwrap();
        assert_eq!(
            sse,
            SseEvent::ToolResult {
                name: "read_file".into(),
                success: true,
            }
        );
    }

    #[tokio::test]
    async fn map_step_event_tool_result_error() {
        use recursive::StepEvent;

        let step_event = StepEvent::ToolResult {
            id: "call_2".into(),
            name: "write_file".into(),
            output: "ERROR: permission denied".into(),
            step: 3,
        };
        let sse = map_step_event(&step_event).unwrap();
        assert_eq!(
            sse,
            SseEvent::ToolResult {
                name: "write_file".into(),
                success: false,
            }
        );
    }

    #[tokio::test]
    async fn map_step_event_finished() {
        use recursive::FinishReason;
        use recursive::StepEvent;

        let step_event = StepEvent::Finished {
            reason: FinishReason::NoMoreToolCalls,
            steps: 5,
        };
        let sse = map_step_event(&step_event).unwrap();
        assert_eq!(
            sse,
            SseEvent::Done {
                finish_reason: "NoMoreToolCalls".into(),
                total_steps: 5,
            }
        );
    }

    #[tokio::test]
    async fn map_step_event_returns_none_for_unrelated() {
        use recursive::StepEvent;

        let step_event = StepEvent::Latency {
            step: 1,
            llm_ms: 500,
        };
        assert!(map_step_event(&step_event).is_none());

        let step_event = StepEvent::PartialToken {
            text: "hello".into(),
            step: 1,
        };
        assert!(map_step_event(&step_event).is_none());
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
}
