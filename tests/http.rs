//! Integration tests for the HTTP API (feature = "http").

#[cfg(feature = "http")]
mod http_tests {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use recursive::http::{AppState, ToolInfo, build_router};
    use recursive::llm::{Completion, MockProvider};
    use recursive::config::Config;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::sync::RwLock;
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
        }
    }

    fn sample_state_with_provider(provider: Arc<MockProvider>) -> AppState {
        AppState {
            tools: vec![],
            config: mock_config(),
            provider,
            sessions: Arc::new(RwLock::new(HashMap::new())),
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
        assert!(resp["finish_reason"].as_str().unwrap().contains("NoMoreToolCalls"));
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
        assert!(resp["finish_reason"].as_str().unwrap().contains("BudgetExceeded"));
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
}
