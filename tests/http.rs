//! Integration tests for the HTTP API (feature = "http").

// Shared fixtures (mock_config / sample_state / sample_state_with_provider /
// SET_INSECURE_OK) live in `tests/http_common/mod.rs`. They were inlined here
// pre-P0-3 and moved out as part of the P0-3 cleanup so the fixtures can be
// reused by future per-feature-area splits of this file. Declared at the file
// root (not inside `mod http_tests`) so `#[path]` resolves relative to
// `tests/http.rs` itself (= `tests/`), exactly like the `tests/invariants/`
// split does. Gated on `feature = "http"` because the fixtures depend on
// `recursive::http` types that only exist under that feature.
#[cfg(feature = "http")]
#[path = "http_common/mod.rs"]
mod common;

#[cfg(feature = "http")]
mod http_tests {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use recursive::http::{
        build_router, build_router_with_auth, build_router_with_auth_and_rate_limit,
        map_agent_event, AppState, AuthConfig, JwtConfig, Metrics, RateLimiter, SessionState,
        SessionUsage, SseEvent, ToolInfo,
    };
    use recursive::llm::{Completion, MockProvider};
    use recursive::runtime::AgentRuntimeBuilder;
    use recursive::tools::ToolRegistry;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::{broadcast, RwLock};
    use tower::ServiceExt;

    use crate::common::{
        mock_config, sample_state, sample_state_with_provider, sample_state_with_storage,
        MemoryStorage, SET_INSECURE_OK,
    };

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

    /// Issue #123: `/healthz` is the k8s-native liveness spelling; it must
    /// behave exactly like `/health`.
    #[tokio::test]
    async fn healthz_returns_ok() {
        let app = build_router(sample_state());

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"ok");
    }

    /// Issue #123: `/readyz` probes storage/LLM/admission and reports them as
    /// JSON (200 when healthy) — unlike the constant `"ok"` of `/health`.
    #[tokio::test]
    async fn readyz_reports_ready_with_checks() {
        let app = build_router(sample_state());

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["ready"], true, "fresh router must be ready: {json}");
        assert_eq!(json["checks"]["storage"]["ok"], true, "{json}");
        assert_eq!(json["checks"]["llm"]["ok"], true, "{json}");
        assert_eq!(json["checks"]["admission"]["saturated"], false, "{json}");
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
        assert!(names.contains(&"Read"));
        assert!(names.contains(&"Write"));
    }

    #[tokio::test]
    async fn tools_empty_state_returns_empty_array() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let app = build_router(AppState {
            tools: vec![],
            config: mock_config(),
            tool_registry: ToolRegistry::local(),
            provider,
            event_channels: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(Metrics::default()),
            slash_commands: Arc::new(Vec::new()),
            host: std::sync::Arc::new(recursive::session_host::SessionHost::new(
                std::time::Duration::from_secs(0),
                recursive::http::AdmissionGate::new(
                    8,
                    std::time::Duration::ZERO,
                    std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                ),
            )),
            rate_limiter: RateLimiter::new(10, 1.0),
            skills: vec![],
            storage: Arc::new(recursive::storage::LocalStorageBackend::new(
                std::env::temp_dir().join(format!("recursive-http-test-{}", std::process::id())),
            )),
            agui_active_runs: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            // Issue #121: no native session mirror for these fixtures.
            session_mirror_root: None,
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
                reasoning_tokens: 0,
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
            event_channels: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(Metrics::default()),
            slash_commands: Arc::new(Vec::new()),
            host: std::sync::Arc::new(recursive::session_host::SessionHost::new(
                std::time::Duration::from_secs(0),
                recursive::http::AdmissionGate::new(
                    8,
                    std::time::Duration::ZERO,
                    std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                ),
            )),
            rate_limiter: RateLimiter::new(10, 1.0),
            skills: vec![],
            storage: Arc::new(recursive::storage::LocalStorageBackend::new(
                std::env::temp_dir().join(format!("recursive-http-test-{}", std::process::id())),
            )),
            agui_active_runs: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            // Issue #121: no native session mirror for these fixtures.
            session_mirror_root: None,
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
            .contains("no_more_tool_calls"));
        assert!(resp["messages"].is_array());
        assert!(!resp["messages"].as_array().unwrap().is_empty());
        assert_eq!(resp["usage"]["total_steps"], 1);
        assert_eq!(resp["usage"]["total_tokens"], 15);
    }

    #[tokio::test]
    async fn run_with_missing_goal_returns_400() {
        // Use `sample_state_with_provider` rather than building AppState
        // inline: the helper also sets `RECURSIVE_HTTP_AUTH_INSECURE_OK=1`
        // so the default-deny auth middleware (G277) lets the request
        // through. Without the env var, the middleware returns 503 and
        // this test never reaches the run_agent handler.
        let provider = Arc::new(MockProvider::new(vec![]));
        let state = sample_state_with_provider(provider);
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

        // Goal 304: ApiError envelope is `{"error": "..."}` — no `status` field.
        assert!(
            resp.get("status").is_none(),
            "ApiError envelope must NOT include a top-level `status` field, got: {resp}"
        );
        assert!(
            resp.get("error").is_some(),
            "ApiError envelope must include an `error` field, got: {resp}"
        );
        assert!(resp["error"].as_str().unwrap().contains("goal"));
    }

    // --- Goal 361: Error → HTTP status-code mapping ---
    //
    // Drive POST /run with a MockProvider that returns a specific typed
    // `recursive::error::Error` on the first LLM call, and assert the
    // response carries the correct status code (previously every variant
    // collapsed to 500).

    #[tokio::test]
    async fn run_returns_403_on_permission_denied() {
        use recursive::error::Error;
        use recursive::permissions::{DecisionReason, PermissionMode};

        let provider =
            Arc::new(
                MockProvider::new(vec![]).with_errors(vec![Error::PermissionDenied {
                    name: "Bash".into(),
                    reason: DecisionReason::Mode(PermissionMode::DontAsk),
                }]),
            );
        let state = sample_state_with_provider(provider);
        let app = build_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/run")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({ "goal": "test" })).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 403);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let resp: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            resp["error"]
                .as_str()
                .unwrap()
                .contains("permission denied"),
            "403 body should explain the denial, got: {resp}"
        );
    }

    /// Issue #100: a transient 429 is retried at the step level, so a run
    /// whose provider recovers completes with 200 instead of surfacing the
    /// error. The 429 → `Retry-After` mapping itself is pinned by the
    /// `map_run_error` unit tests in `src/http/handlers.rs`.
    #[tokio::test]
    async fn run_recovers_from_transient_rate_limit() {
        use recursive::error::Error;

        let provider = Arc::new(
            MockProvider::new(vec![Completion {
                content: "recovered".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            }])
            .with_errors(vec![Error::RateLimited {
                provider: "mock".into(),
                retry_after_ms: 1,
                request_id: None,
            }]),
        );
        let state = sample_state_with_provider(provider);
        let app = build_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/run")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({ "goal": "test" })).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let resp: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(resp["status"], "success");
        assert!(
            resp["messages"].to_string().contains("recovered"),
            "the retried completion must land in the transcript, got: {resp}"
        );
    }

    #[tokio::test]
    async fn run_returns_400_on_bad_tool_args() {
        use recursive::error::Error;

        let provider = Arc::new(
            MockProvider::new(vec![]).with_errors(vec![Error::BadToolArgs {
                name: "Read".into(),
                message: "missing path".into(),
            }]),
        );
        let state = sample_state_with_provider(provider);
        let app = build_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/run")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({ "goal": "test" })).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 400);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let resp: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            resp["error"]
                .as_str()
                .unwrap()
                .contains("bad tool arguments"),
            "400 body should explain the bad arguments, got: {resp}"
        );
    }

    #[tokio::test]
    async fn run_returns_500_on_internal_llm_error() {
        use recursive::error::Error;

        // Regression guard: a genuine provider failure (e.g. upstream 5xx)
        // must stay 500 — the mapping must not accidentally 4xx real
        // server-side failures.
        let provider = Arc::new(MockProvider::new(vec![]).with_errors(vec![Error::Llm {
            provider: "mock".into(),
            model: None,
            request_id: None,
            message: "upstream 5xx".into(),
        }]));
        let state = sample_state_with_provider(provider);
        let app = build_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/run")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({ "goal": "test" })).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 500);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let resp: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            resp["error"].as_str().unwrap().contains("LLM error"),
            "500 body should expose the LLM failure, got: {resp}"
        );
    }

    #[tokio::test]
    async fn run_response_has_expected_fields() {
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "Done.".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: Some(recursive::llm::TokenUsage {
                reasoning_tokens: 0,
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
            event_channels: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(Metrics::default()),
            slash_commands: Arc::new(Vec::new()),
            host: std::sync::Arc::new(recursive::session_host::SessionHost::new(
                std::time::Duration::from_secs(0),
                recursive::http::AdmissionGate::new(
                    8,
                    std::time::Duration::ZERO,
                    std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                ),
            )),
            rate_limiter: RateLimiter::new(10, 1.0),
            skills: vec![],
            storage: Arc::new(recursive::storage::LocalStorageBackend::new(
                std::env::temp_dir().join(format!("recursive-http-test-{}", std::process::id())),
            )),
            agui_active_runs: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            // Issue #121: no native session mirror for these fixtures.
            session_mirror_root: None,
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
            event_channels: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(Metrics::default()),
            slash_commands: Arc::new(Vec::new()),
            host: std::sync::Arc::new(recursive::session_host::SessionHost::new(
                std::time::Duration::from_secs(0),
                recursive::http::AdmissionGate::new(
                    8,
                    std::time::Duration::ZERO,
                    std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                ),
            )),
            rate_limiter: RateLimiter::new(10, 1.0),
            skills: vec![],
            storage: Arc::new(recursive::storage::LocalStorageBackend::new(
                std::env::temp_dir().join(format!("recursive-http-test-{}", std::process::id())),
            )),
            agui_active_runs: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            // Issue #121: no native session mirror for these fixtures.
            session_mirror_root: None,
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
            event_channels: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(Metrics::default()),
            slash_commands: Arc::new(Vec::new()),
            host: std::sync::Arc::new(recursive::session_host::SessionHost::new(
                std::time::Duration::from_secs(0),
                recursive::http::AdmissionGate::new(
                    8,
                    std::time::Duration::ZERO,
                    std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                ),
            )),
            rate_limiter: RateLimiter::new(10, 1.0),
            skills: vec![],
            storage: Arc::new(recursive::storage::LocalStorageBackend::new(
                std::env::temp_dir().join(format!("recursive-http-test-{}", std::process::id())),
            )),
            agui_active_runs: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            // Issue #121: no native session mirror for these fixtures.
            session_mirror_root: None,
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

        // Should hit budget exceeded at 2 steps. Issue #113: `status` reports
        // the terminal finish reason — it used to be a hardcoded "success".
        assert_eq!(resp["status"], "budget_exceeded");
        assert!(resp["finish_reason"]
            .as_str()
            .unwrap()
            .contains("budget_exceeded"));
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
        // Goal-293: GET /sessions now returns a `{ total, sessions }` envelope.
        let resp: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(resp["total"], 1);
        let sessions = resp["sessions"]
            .as_array()
            .expect("sessions should be an array");
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0]["id"].is_string());
        assert_eq!(sessions[0]["message_count"], 0);
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

    /// Goal 398: with the run pool saturated, `POST /sessions/:id/messages`
    /// must wait the admission window and then fail fast with `503` +
    /// an integer `Retry-After` in the standard ApiError body shape —
    /// and succeed end-to-end once a permit is released.
    #[tokio::test]
    async fn messages_returns_503_with_retry_after_when_admission_saturated() {
        SET_INSECURE_OK.call_once(|| {
            unsafe { std::env::set_var("RECURSIVE_HTTP_AUTH_INSECURE_OK", "1") };
        });
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "done".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let metrics = Arc::new(Metrics::default());
        let state = AppState {
            tools: vec![],
            config: mock_config(),
            tool_registry: ToolRegistry::local(),
            provider,
            event_channels: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::clone(&metrics),
            slash_commands: Arc::new(Vec::new()),
            host: std::sync::Arc::new(recursive::session_host::SessionHost::new(
                std::time::Duration::from_secs(0),
                recursive::http::AdmissionGate::new(
                    1,
                    std::time::Duration::from_millis(150),
                    Arc::clone(&metrics.runs_waiting),
                    Arc::clone(&metrics.runs_in_flight),
                ),
            )),
            rate_limiter: RateLimiter::new(10, 1.0),
            skills: vec![],
            storage: Arc::new(recursive::storage::LocalStorageBackend::new(
                std::env::temp_dir().join(format!("recursive-http-test-{}", std::process::id())),
            )),
            agui_active_runs: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            // Issue #121: no native session mirror for these fixtures.
            session_mirror_root: None,
        };

        // Create a session.
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
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            status,
            201,
            "session creation failed: {}",
            String::from_utf8_lossy(&body)
        );
        let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let session_id = created["id"].as_str().unwrap().to_string();

        // Saturate the single run slot directly through the gate.
        let hold = state.host.admission().acquire_run().await.unwrap();
        assert!(state.host.admission().try_acquire_run().is_err());

        // The message request now queues, times out, and gets a 503.
        let app = build_router(state.clone());
        let start = std::time::Instant::now();
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/sessions/{}/messages", session_id))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({"content": "Hi"})).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let elapsed = start.elapsed();

        assert_eq!(response.status(), 503);
        // It waited for the admission window (not a fast-fail on another path).
        assert!(
            elapsed >= std::time::Duration::from_millis(140),
            "should wait ~150ms before 503, got {elapsed:?}"
        );
        // Retry-After must be parseable integer seconds (no floats/ranges).
        let retry_after = response
            .headers()
            .get("retry-after")
            .expect("503 must carry Retry-After")
            .to_str()
            .unwrap()
            .to_owned();
        let secs: u32 = retry_after
            .parse()
            .expect("Retry-After must be integer seconds");
        assert!(secs >= 1, "Retry-After must be >= 1, got {secs}");
        // Body follows the standard ApiError envelope.
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let err: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            err["error"].is_string(),
            "body must be ApiError shape: {err}"
        );
        // The timed-out waiter released its queue slot.
        assert_eq!(
            metrics
                .runs_waiting
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );

        // Release the permit; the same request now succeeds end-to-end.
        drop(hold);
        let app = build_router(state);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/sessions/{}/messages", session_id))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({"content": "Hi"})).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
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

    /// Goal-310: delete_session must clean up event_channels.
    #[tokio::test]
    async fn delete_session_cleans_up_event_channels() {
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "Hello!".into(),
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
        let session_id = create_resp["id"].as_str().unwrap().to_string();

        // Send a message to trigger event channel creation.
        let app = build_router(state.clone());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/sessions/{}/messages", session_id))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({
                            "content": "Hi"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);

        // Verify event channel entry exists.
        assert!(
            state.event_channels.read().await.contains_key(&session_id),
            "event channel entry must exist after sending a message"
        );

        // Delete the session.
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

        // Verify event channel entry is removed.
        assert!(
            !state.event_channels.read().await.contains_key(&session_id),
            "event channel entry must be removed after session deletion"
        );
    }

    /// Goal 396: DELETE must persist the session transcript through the
    /// storage backend, and the persisted transcript must round-trip via
    /// `load_transcript` with tool-call ↔ tool-result pairing intact
    /// (invariant #8).
    #[tokio::test]
    async fn delete_session_persists_transcript_with_tool_pairing() {
        use recursive::llm::ToolCall;
        use recursive::message::Role;
        use recursive::storage::StorageBackend;

        let storage = MemoryStorage::new();
        let provider = Arc::new(MockProvider::new(vec![
            // Step 1: the model calls a tool.
            Completion {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "unknown".into(),
                    arguments: serde_json::json!({ "path": "x.txt" }),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            // Step 2: after the tool result, the model answers.
            Completion {
                content: "done".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));
        let state = AppState {
            tools: vec![],
            config: mock_config(),
            tool_registry: ToolRegistry::local(),
            provider,
            host: std::sync::Arc::new(recursive::session_host::SessionHost::new(
                std::time::Duration::from_secs(0),
                recursive::http::AdmissionGate::new(
                    8,
                    std::time::Duration::ZERO,
                    std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                ),
            )),
            event_channels: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(Metrics::default()),
            slash_commands: Arc::new(Vec::new()),
            rate_limiter: RateLimiter::new(10, 1.0),
            skills: vec![],
            storage: storage.clone(),
            agui_active_runs: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            // Issue #121: no native session mirror for these fixtures.
            session_mirror_root: None,
        };

        // Create a session.
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
        assert_eq!(response.status(), 201);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let create_resp: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let session_id = create_resp["id"].as_str().unwrap().to_string();

        // Send one turn that exercises a tool call + tool result.
        let app = build_router(state.clone());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/sessions/{}/messages", session_id))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({
                            "content": "read x.txt"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);

        // Delete the session — the persistence trigger.
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

        // Exactly one save, for THIS session.
        let saves = storage.saves();
        assert_eq!(saves.len(), 1, "DELETE must trigger exactly one save");
        assert_eq!(saves[0].session_id, session_id);
        assert_eq!(
            saves[0].probe_lock_was_free, None,
            "no probe attached in this test"
        );

        // The persisted transcript round-trips via load_transcript.
        let loaded = storage.load_transcript(&session_id).await.unwrap();
        assert!(!loaded.is_empty(), "persisted transcript must be non-empty");
        assert_eq!(loaded, saves[0].messages, "round-trip must be lossless");

        // Invariant #8: every Tool message pairs with the assistant
        // tool_calls immediately before it.
        let mut open_tool_call_ids: Vec<String> = Vec::new();
        let mut saw_tool_pair = false;
        for msg in &loaded {
            match msg.role {
                Role::Assistant => {
                    open_tool_call_ids = msg.tool_calls.iter().map(|c| c.id.clone()).collect();
                }
                Role::Tool => {
                    let id = msg.tool_call_id.as_deref().unwrap_or_default();
                    assert!(
                        open_tool_call_ids.iter().any(|c| c == id),
                        "tool message with call id {id:?} has no matching assistant tool_call"
                    );
                    saw_tool_pair = true;
                }
                _ => {}
            }
        }
        assert!(
            saw_tool_pair,
            "the persisted transcript must contain a tool-call/tool-result pair"
        );
    }

    /// Goal 396: two sessions deleted back-to-back must persist their OWN
    /// transcripts — no cross-session bleed.
    #[tokio::test]
    async fn delete_persists_each_session_transcript_separately() {
        // One scripted completion per session turn — the provider is shared
        // across both sessions, so a single entry would starve the second.
        let provider = Arc::new(MockProvider::new(vec![
            Completion {
                content: "reply one".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "reply two".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));
        let storage = MemoryStorage::new();
        let state = sample_state_with_provider(provider);
        // Swap in the recording backend (fixture uses a plain one).
        let state = AppState {
            storage: storage.clone(),
            agui_active_runs: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            // Issue #121: no native session mirror for these fixtures.
            session_mirror_root: None,
            ..state
        };

        let mut ids = Vec::new();
        for _ in 0..2 {
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
            assert_eq!(response.status(), 201);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let create_resp: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let session_id = create_resp["id"].as_str().unwrap().to_string();

            // One turn per session with distinct content.
            let app = build_router(state.clone());
            let response = app
                .oneshot(
                    axum::http::Request::builder()
                        .method("POST")
                        .uri(format!("/sessions/{}/messages", session_id))
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::to_string(&serde_json::json!({
                                "content": session_id
                            }))
                            .unwrap(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            ids.push(session_id);
        }

        // Delete both.
        for id in &ids {
            let app = build_router(state.clone());
            let response = app
                .oneshot(
                    axum::http::Request::builder()
                        .method("DELETE")
                        .uri(format!("/sessions/{}", id))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), 204);
        }

        let saves = storage.saves();
        assert_eq!(saves.len(), 2, "one save per deleted session");
        let mut saved_ids: Vec<&str> = saves.iter().map(|s| s.session_id.as_str()).collect();
        saved_ids.sort();
        let mut expected = ids.clone();
        expected.sort();
        assert_eq!(saved_ids, expected, "saves must be keyed by session id");

        // Each saved transcript contains its own session's prompt, not the
        // other session's.
        for (i, id) in ids.iter().enumerate() {
            let record = saves
                .iter()
                .find(|s| s.session_id == *id)
                .expect("save for this session");
            let user_texts: Vec<&str> = record
                .messages
                .iter()
                .filter(|m| m.role == recursive::message::Role::User)
                .map(|m| m.content.as_str())
                .collect();
            assert!(
                user_texts.contains(&id.as_str()),
                "session {id} save must contain its own prompt, got {user_texts:?}"
            );
            let other: Vec<&String> = ids.iter().filter(|o| *o != id).collect();
            for o in other {
                assert!(
                    !record.messages.iter().any(|m| m.content == *o),
                    "session {id} save must not contain the other session's prompt {o}"
                );
            }
            let _ = i;
        }
    }

    /// Issue #102: `DELETE /sessions/:id?purge=true` is the true-delete path.
    /// The default DELETE keeps the Goal-396 snapshot behind a tombstone;
    /// purge erases both — and works when the session is no longer live, so a
    /// data-subject deletion request can still reach a persisted copy.
    #[tokio::test]
    async fn delete_session_with_purge_erases_snapshot_and_tombstone() {
        use recursive::storage::StorageBackend;

        let storage = MemoryStorage::new();
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "hello".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let state = sample_state_with_storage(provider, storage.clone());

        // Create a session with a real transcript.
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
        let session_id = serde_json::from_slice::<serde_json::Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let app = build_router(state.clone());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/sessions/{session_id}/messages"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({"content": "hi"})).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let tombstone = format!("session-deleted/{session_id}");

        // Default DELETE: snapshot persisted, tombstone written.
        let app = build_router(state.clone());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri(format!("/sessions/{session_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 204);
        assert!(
            storage.deleted().is_empty(),
            "a default DELETE must not erase the snapshot (Goal 396)"
        );
        assert!(storage.has_memory(&tombstone), "tombstone written");
        assert!(!storage
            .load_transcript(&session_id)
            .await
            .unwrap()
            .is_empty());

        // Purge DELETE: the session is no longer live, yet every persisted
        // copy must go.
        let app = build_router(state.clone());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri(format!("/sessions/{session_id}?purge=true"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 204);

        assert_eq!(
            storage.deleted(),
            vec![session_id.clone()],
            "purge must delete the persisted transcript"
        );
        assert!(
            storage
                .load_transcript(&session_id)
                .await
                .unwrap()
                .is_empty(),
            "no transcript copy may survive a purge"
        );
        assert!(
            !storage.has_memory(&tombstone),
            "no tombstone naming the session may survive a purge"
        );
    }

    /// Issue #92: a turn persists its transcript immediately — without
    /// waiting for DELETE / eviction / shutdown — so a crashed gateway loses
    /// at most the in-flight turn.
    #[tokio::test]
    async fn post_message_appends_transcript_before_teardown() {
        use recursive::storage::StorageBackend;

        let storage = MemoryStorage::new();
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "hi back".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let state = sample_state_with_provider(provider);
        let state = AppState {
            storage: storage.clone(),
            agui_active_runs: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            ..state
        };

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
        assert_eq!(response.status(), 201);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let session_id = serde_json::from_slice::<serde_json::Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        let app = build_router(state.clone());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/sessions/{session_id}/messages"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({ "content": "hello" })).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);

        // No DELETE: the turn's transcript must already be on the backend.
        let stored = storage.load_transcript(&session_id).await.unwrap();
        assert!(
            stored.iter().any(|m| m.content == "hi back"),
            "the assistant reply must be persisted before teardown, got {stored:?}"
        );
        assert!(
            storage.saves().is_empty(),
            "the per-turn path must append, not full-save"
        );
        assert!(
            !storage.appends().is_empty(),
            "the turn must record at least one append"
        );
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

    /// Issue #147: a REST session's native mirror (#121) is written as the
    /// turn goes, not only at teardown — a host killed mid-turn keeps the
    /// steps that already completed.
    #[tokio::test]
    async fn post_message_mirrors_the_session_transcript_before_teardown() {
        let mirror_root = tempfile::tempdir().expect("mirror root");
        let storage = MemoryStorage::new();
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "mirrored reply".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let base = sample_state_with_provider(provider);
        let workspace = base.config.workspace.clone();
        let state = AppState {
            storage: storage.clone(),
            agui_active_runs: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            session_mirror_root: Some(mirror_root.path().to_path_buf()),
            ..base
        };

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
        assert_eq!(response.status(), 201);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let session_id = serde_json::from_slice::<serde_json::Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        let app = build_router(state.clone());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/sessions/{session_id}/messages"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({ "content": "hello mirror" }))
                            .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);

        // No DELETE / eviction / shutdown: the mirror must already hold the
        // turn, one row per completed step.
        let dir = std::fs::read_dir(mirror_root.path())
            .expect("mirror root")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path().join(&session_id))
            .find(|candidate| candidate.is_dir())
            .unwrap_or_else(|| panic!("no mirror directory for {session_id} under {workspace:?}"));
        let entries =
            recursive::session::SessionReader::load_transcript(&dir).expect("mirror transcript");
        let contents: Vec<&str> = entries.iter().map(|e| e.content.as_str()).collect();
        assert!(
            contents.contains(&"hello mirror"),
            "the user turn must be mirrored before teardown, got {contents:?}"
        );
        assert!(
            contents.contains(&"mirrored reply"),
            "the assistant reply must be mirrored before teardown, got {contents:?}"
        );
        assert_eq!(
            contents.iter().filter(|c| **c == "hello mirror").count(),
            1,
            "no row may be mirrored twice, got {contents:?}"
        );
        assert_eq!(
            contents.iter().filter(|c| **c == "mirrored reply").count(),
            1,
            "no row may be mirrored twice, got {contents:?}"
        );
    }

    /// Goal-297: PATCH /sessions/:id must echo the actual non-system
    /// message count, not 0. `SessionState::non_system_message_count`
    /// is an `Arc<AtomicUsize>` kept in lock-step with the runtime's
    /// transcript by the SSE forwarder; the patch handler reads it
    /// directly instead of returning a placeholder.
    #[tokio::test]
    async fn patch_session_returns_actual_message_count() {
        // One canned response for the assistant turn.
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "hi".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let state = sample_state_with_provider(provider);
        let app = build_router(state);

        // 1) Create a session (count starts at 0).
        let response = app
            .clone()
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
        let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let session_id = created["id"].as_str().unwrap().to_string();

        // 2) Send a user message so the forwarder increments the
        //    atomic to 2 (user + assistant). By the time send_session_message
        //    returns 200 it has awaited the forwarder task to completion.
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/sessions/{session_id}/messages"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({
                            "content": "hi"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "send_message must succeed");

        // 3) PATCH the session title.
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("PATCH")
                    .uri(format!("/sessions/{session_id}"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"title":"rename me"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "PATCH must succeed");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let patched: serde_json::Value = serde_json::from_slice(&body).unwrap();

        // 4) message_count must be the actual count (2), not 0.
        assert_eq!(
            patched["message_count"], 2,
            "patch_session must return the real non-system message count, got: {patched}"
        );
        assert_eq!(
            patched["title"], "rename me",
            "title must reflect the patched value, got: {patched}"
        );
        assert_eq!(
            patched["id"], session_id,
            "id must round-trip, got: {patched}"
        );
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

    // ── Goal 295: standardized JSON error body ───────────────────────────

    /// Every 4xx/5xx error response produced by an `ApiError` site must
    /// carry a JSON body of exactly `{"error": "<message>"}` so clients
    /// can always parse the failure as JSON. This pins the contract for
    /// the most common case (`GET /sessions/:id` with a missing id) plus
    /// the sibling handlers (DELETE/PATCH/fork/events).
    #[tokio::test]
    async fn api_error_sites_return_json_error_body() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let state = sample_state_with_provider(provider);
        let app = build_router(state);

        // ── GET /sessions/:id (404 → ApiError::not_found) ────────────────
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/sessions/nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 404, "GET missing session must 404");
        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value =
            serde_json::from_slice(&body_bytes).expect("404 body must be valid JSON");
        assert!(
            body.get("error").is_some(),
            "404 body must have an \"error\" key, got: {body}"
        );
        assert!(
            body["error"].as_str().is_some(),
            "404 body \"error\" must be a string, got: {body}"
        );
        let error_msg = body["error"].as_str().unwrap();
        assert!(
            error_msg.contains("session") && error_msg.contains("not found"),
            "404 error message should mention session not found, got: {error_msg}"
        );

        // ── DELETE /sessions/:id (404 → ApiError::not_found) ─────────────
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri("/sessions/nonexistent-delete")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 404, "DELETE missing session must 404");
        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value =
            serde_json::from_slice(&body_bytes).expect("404 body must be valid JSON");
        assert!(
            body.get("error").is_some(),
            "DELETE 404 body must have \"error\" key, got: {body}"
        );

        // ── PATCH /sessions/:id (404 → ApiError::not_found) ──────────────
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("PATCH")
                    .uri("/sessions/nonexistent-patch")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"title":"x"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 404, "PATCH missing session must 404");
        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value =
            serde_json::from_slice(&body_bytes).expect("404 body must be valid JSON");
        assert!(
            body.get("error").is_some(),
            "PATCH 404 body must have \"error\" key, got: {body}"
        );

        // ── POST /sessions/:id/fork (404 → ApiError::not_found) ──────────
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/sessions/nonexistent-fork/fork")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 404, "FORK missing session must 404");
        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value =
            serde_json::from_slice(&body_bytes).expect("404 body must be valid JSON");
        assert!(
            body.get("error").is_some(),
            "FORK 404 body must have \"error\" key, got: {body}"
        );

        // ── GET /sessions/:id/events (404 → ApiError::not_found) ─────────
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/sessions/nonexistent-events/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 404, "EVENTS missing session must 404");
        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value =
            serde_json::from_slice(&body_bytes).expect("404 body must be valid JSON");
        assert!(
            body.get("error").is_some(),
            "EVENTS 404 body must have \"error\" key, got: {body}"
        );
    }

    // ── Goal 304: G295-migrated handlers emit the `{"error": "..."}` envelope ──

    /// Pin the G304 contract: the three handlers that still used the
    /// legacy `(StatusCode, Json<ErrorResponse>)` tuple in G295
    /// (`run_agent`, `create_session`, `send_session_message`) must
    /// emit the standardized `{"error": "<message>"}` envelope on
    /// every 4xx/5xx response — never the legacy `{"status","error"}`
    /// shape from `ErrorResponse`. We pin the error sites we can
    /// reach from the outside (empty goal, missing session) and the
    /// success path of `POST /sessions` (whose only error site,
    /// runtime build failure, requires an internal mock to trigger
    /// and is covered indirectly by the test passing).
    #[tokio::test]
    async fn goal_304_run_sessions_sessions_messages_emit_api_error_envelope() {
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "ok".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let state = sample_state_with_provider(provider);
        let app = build_router(state);

        // ── POST /run: empty goal → 400, `{"error": "..."}`, no `status` field.
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/run")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({"goal": ""})).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "POST /run with empty goal must 400");
        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value =
            serde_json::from_slice(&body_bytes).expect("400 body must be valid JSON");
        assert!(
            body.get("status").is_none(),
            "POST /run ApiError envelope must NOT include a `status` field, got: {body}"
        );
        assert!(
            body.get("error").is_some(),
            "POST /run ApiError envelope must include an `error` field, got: {body}"
        );
        assert!(
            body["error"].as_str().unwrap().contains("goal"),
            "POST /run error message should mention `goal`, got: {body}"
        );

        // ── POST /sessions: success path. The single error site in
        //    `create_session` is `AgentRuntimeBuilder::build()` failure
        //    (500), which is not reachable from the HTTP input alone,
        //    so we pin the SUCCESS path here: the response must NOT
        //    include a `status` field on the 201 success body.
        let resp = app
            .clone()
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
        assert_eq!(resp.status(), 201, "POST /sessions must 201 on success");
        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value =
            serde_json::from_slice(&body_bytes).expect("201 body must be valid JSON");
        assert!(
            body.get("error").is_none(),
            "POST /sessions success body must NOT include an `error` field, got: {body}"
        );
        assert!(
            body.get("id").is_some(),
            "POST /sessions success body must include `id`, got: {body}"
        );

        // ── POST /sessions/:id/messages: missing session → 404, `{"error": "..."}`.
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/sessions/no-such-session-id/messages")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({
                            "content": "hi"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            404,
            "POST /sessions/:id/messages with missing session must 404"
        );
        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value =
            serde_json::from_slice(&body_bytes).expect("404 body must be valid JSON");
        assert!(
            body.get("status").is_none(),
            "POST /sessions/:id/messages ApiError envelope must NOT include a `status` field, got: {body}"
        );
        assert!(
            body.get("error").is_some(),
            "POST /sessions/:id/messages ApiError envelope must include an `error` field, got: {body}"
        );
        let err = body["error"].as_str().unwrap();
        assert!(
            err.contains("session") && err.contains("not found"),
            "404 error message should mention session not found, got: {err}"
        );
    }

    // ── Goal 313: G295-migrated handlers (plan/goal/interrupt) emit the
    //    standardized `{"error": "..."}` envelope. Mirrors the G304 test
    //    for plan_confirm / plan_reject / session_set_goal /
    //    session_interrupt. The G304 test already covers
    //    `run_agent` / `create_session` / `send_session_message`.

    /// Pin the G313 contract: `POST /sessions/:id/plan/confirm` against
    /// a nonexistent session_id returns 404 with `{"error": "..."}` and
    /// NO `status` field (the standardized ApiError envelope).
    #[tokio::test]
    async fn plan_confirm_returns_api_error_envelope_on_404() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let state = sample_state_with_provider(provider);
        let app = build_router(state);

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/sessions/nonexistent-plan-confirm/plan/confirm")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), 404, "missing session must 404");
        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value =
            serde_json::from_slice(&body_bytes).expect("404 body must be valid JSON");
        assert!(
            body.get("status").is_none(),
            "ApiError envelope must NOT include a `status` field, got: {body}"
        );
        let err = body["error"]
            .as_str()
            .expect("ApiError envelope must have a string `error` field");
        assert!(
            !err.is_empty(),
            "ApiError envelope `error` must be non-empty, got: {body}"
        );
        assert!(
            err.contains("session") && err.contains("not found"),
            "404 error should mention session not found, got: {err}"
        );
    }

    /// Pin the G313 contract: `POST /sessions/:id/plan/reject` against
    /// a nonexistent session_id returns 404 with `{"error": "..."}`.
    #[tokio::test]
    async fn plan_reject_returns_api_error_envelope_on_404() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let state = sample_state_with_provider(provider);
        let app = build_router(state);

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/sessions/nonexistent-plan-reject/plan/reject")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"reason":"missing"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), 404, "missing session must 404");
        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value =
            serde_json::from_slice(&body_bytes).expect("404 body must be valid JSON");
        assert!(
            body.get("status").is_none(),
            "ApiError envelope must NOT include a `status` field, got: {body}"
        );
        let err = body["error"]
            .as_str()
            .expect("ApiError envelope must have a string `error` field");
        assert!(
            !err.is_empty(),
            "ApiError envelope `error` must be non-empty, got: {body}"
        );
        assert!(
            err.contains("session") && err.contains("not found"),
            "404 error should mention session not found, got: {err}"
        );
    }

    /// Pin the G313 contract: `POST /sessions/:id/goal` against a
    /// nonexistent session_id returns 404 with `{"error": "..."}`.
    #[tokio::test]
    async fn session_set_goal_returns_api_error_envelope_on_404() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let state = sample_state_with_provider(provider);
        let app = build_router(state);

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/sessions/nonexistent-set-goal/goal")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"condition":"something"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), 404, "missing session must 404");
        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value =
            serde_json::from_slice(&body_bytes).expect("404 body must be valid JSON");
        assert!(
            body.get("status").is_none(),
            "ApiError envelope must NOT include a `status` field, got: {body}"
        );
        let err = body["error"]
            .as_str()
            .expect("ApiError envelope must have a string `error` field");
        assert!(
            !err.is_empty(),
            "ApiError envelope `error` must be non-empty, got: {body}"
        );
        assert!(
            err.contains("session") && err.contains("not found"),
            "404 error should mention session not found, got: {err}"
        );
    }

    /// Pin the G313 contract: `POST /sessions/:id/interrupt` against a
    /// nonexistent session_id returns 404 with `{"error": "..."}`.
    #[tokio::test]
    async fn session_interrupt_returns_api_error_envelope_on_404() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let state = sample_state_with_provider(provider);
        let app = build_router(state);

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/sessions/nonexistent-interrupt/interrupt")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), 404, "missing session must 404");
        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value =
            serde_json::from_slice(&body_bytes).expect("404 body must be valid JSON");
        assert!(
            body.get("status").is_none(),
            "ApiError envelope must NOT include a `status` field, got: {body}"
        );
        let err = body["error"]
            .as_str()
            .expect("ApiError envelope must have a string `error` field");
        assert!(
            !err.is_empty(),
            "ApiError envelope `error` must be non-empty, got: {body}"
        );
        assert!(
            err.contains("session") && err.contains("not found"),
            "404 error should mention session not found, got: {err}"
        );
    }

    /// Pin the G313 contract: 409 CONFLICT responses on the migrated
    /// handlers must also use the standardized `{"error": "..."}`
    /// envelope. This covers the two 409 sites reachable without a
    /// real agent turn:
    ///   - `plan_confirm` when no plan is pending ("session is not
    ///     awaiting plan approval").
    ///   - `set_goal` when the runtime Mutex is held by an in-flight
    ///     turn ("session runtime is busy").
    #[tokio::test]
    async fn goal_313_plan_and_set_goal_conflicts_use_api_error_envelope() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let state = sample_state_with_provider(provider);
        let app = build_router(state.clone());

        // Create a session — needed for the plan_confirm 409 case (the
        // session exists but has no pending plan).
        let create_resp = app
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
        assert_eq!(create_resp.status(), 201);
        let body = create_resp.into_body().collect().await.unwrap().to_bytes();
        let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let session_id = created["id"].as_str().unwrap().to_string();

        // ── plan_confirm 409 ──────────────────────────────────────────
        let app = build_router(state.clone());
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
        assert_eq!(
            resp.status(),
            409,
            "plan_confirm with no pending plan must 409"
        );
        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value =
            serde_json::from_slice(&body_bytes).expect("409 body must be valid JSON");
        assert!(
            body.get("status").is_none(),
            "409 ApiError envelope must NOT include a `status` field, got: {body}"
        );
        let err = body["error"]
            .as_str()
            .expect("409 envelope must have a string `error` field");
        assert!(
            err.contains("plan approval"),
            "409 error should mention plan approval, got: {err}"
        );

        // ── set_goal 409 — pin the JSON envelope (the runtime is
        //    *not* busy in this fixture, so set_goal returns 200; we
        //    verify the body shape on the SUCCESS path instead, and
        //    cover the 409 envelope shape via the lower-level ApiError
        //    unit-level assertions in the per-handler tests above).
        //    To exercise the 409 we acquire the runtime Mutex first;
        //    that requires touching AppState directly.
        let runtime_arc = {
            let sessions = state.host.sessions();
            let sessions = sessions.read().await;
            sessions.get(&session_id).unwrap().runtime.clone()
        };
        let _guard = runtime_arc.lock().await;

        let app = build_router(state.clone());
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/sessions/{session_id}/goal"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"condition":"something"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 409, "set_goal with busy runtime must 409");
        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value =
            serde_json::from_slice(&body_bytes).expect("409 body must be valid JSON");
        assert!(
            body.get("status").is_none(),
            "set_goal 409 ApiError envelope must NOT include a `status` field, got: {body}"
        );
        let err = body["error"]
            .as_str()
            .expect("set_goal 409 envelope must have a string `error` field");
        assert!(
            !err.is_empty(),
            "set_goal 409 envelope `error` must be non-empty, got: {body}"
        );
        assert!(
            err.contains("busy"),
            "set_goal 409 error should mention busy, got: {err}"
        );
    }

    /// Pin the G313 contract: `DELETE /sessions/:id/goal` (clear_goal)
    /// returns 409 with the standardized `{"error": "..."}` envelope
    /// AND preserves the `Retry-After: 5` header AND keeps the old
    /// `retry after` hint text in the error message.
    #[tokio::test]
    async fn goal_313_clear_goal_409_envelope_preserves_retry_after_hint() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let state = sample_state_with_provider(provider);
        let app = build_router(state.clone());

        // Create a session.
        let create_resp = app
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
        assert_eq!(create_resp.status(), 201);
        let body = create_resp.into_body().collect().await.unwrap().to_bytes();
        let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let session_id = created["id"].as_str().unwrap().to_string();

        // Hold the runtime Mutex so clear_goal hits its 409 path.
        let runtime_arc = {
            let sessions = state.host.sessions();
            let sessions = sessions.read().await;
            sessions.get(&session_id).unwrap().runtime.clone()
        };
        let _guard = runtime_arc.lock().await;

        let app = build_router(state.clone());
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri(format!("/sessions/{session_id}/goal"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 409, "clear_goal with busy runtime must 409");

        // Retry-After header must still be set so clients can back off.
        // Check the headers BEFORE consuming the body (into_body moves).
        let retry_after = resp
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .expect("Retry-After header missing on clear_goal 409")
            .to_str()
            .unwrap();
        assert_eq!(
            retry_after, "5",
            "Retry-After must stay at 5s for clear_goal 409"
        );

        // Standardized envelope — `{"error": "..."}`, no legacy `status` field.
        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value =
            serde_json::from_slice(&body_bytes).expect("409 body must be valid JSON");
        assert!(
            body.get("status").is_none(),
            "clear_goal 409 ApiError envelope must NOT include a `status` field, got: {body}"
        );
        let err = body["error"]
            .as_str()
            .expect("clear_goal 409 envelope must have a string `error` field");
        // Goal-313: the original `hint` text must survive in the error
        // message even though we dropped the separate `hint`/`session_id`
        // top-level keys (so clients can still parse a uniform envelope).
        assert!(
            err.contains("retry after"),
            "clear_goal 409 error must preserve the original hint text, got: {err}"
        );
    }

    #[tokio::test]
    async fn sse_event_serialization() {
        // Verify SseEvent serializes to expected JSON structure
        let tool_call = SseEvent::ToolCall {
            name: "Read".into(),
            step: 1,
        };
        let json = serde_json::to_value(&tool_call).unwrap();
        assert_eq!(json["type"], "tool_call");
        assert_eq!(json["name"], "Read");
        assert_eq!(json["step"], 1);

        let tool_result = SseEvent::ToolResult {
            name: "Read".into(),
            success: true,
        };
        let json = serde_json::to_value(&tool_result).unwrap();
        assert_eq!(json["type"], "tool_result");
        assert_eq!(json["name"], "Read");
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
            name: "Write".into(),
            id: "call_1".into(),
            arguments: r#"{"path": "/tmp/test"}"#.into(),
            step: 2,
        };
        let sse = map_agent_event(&event).unwrap();
        assert_eq!(
            sse,
            SseEvent::ToolCall {
                name: "Write".into(),
                step: 2,
            }
        );
    }

    #[tokio::test]
    async fn map_agent_event_tool_result_success() {
        use recursive::AgentEvent;

        let event = AgentEvent::ToolResult {
            id: "call_1".into(),
            name: "Read".into(),
            output: "file contents here".into(),
            step: 1,
            is_error: false,
            duration_ms: 0,
        };
        let sse = map_agent_event(&event).unwrap();
        assert_eq!(
            sse,
            SseEvent::ToolResult {
                name: "Read".into(),
                success: true,
            }
        );
    }

    #[tokio::test]
    async fn map_agent_event_tool_result_error() {
        use recursive::AgentEvent;

        let event = AgentEvent::ToolResult {
            id: "call_2".into(),
            name: "Write".into(),
            output: "ERROR: permission denied".into(),
            step: 3,
            is_error: true,
            duration_ms: 0,
        };
        let sse = map_agent_event(&event).unwrap();
        assert_eq!(
            sse,
            SseEvent::ToolResult {
                name: "Write".into(),
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
                is_compaction_summary: false,
            },
            usage: None,
            step: None,
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
                    name: "Read".into(),
                    arguments: serde_json::json!({"path": "x"}),
                }],
                tool_call_id: None,
                reasoning_content: None,
                is_compaction_summary: false,
            },
            usage: None,
            step: None,
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
            SseContentBlock::ToolUse { id, name, .. } if id == "tc1" && name == "Read"
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
                    is_compaction_summary: false,
                },
                usage: None,
                step: None,
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

    /// Issue #102: the delete operation must document the `purge` query flag —
    /// clients discover the true-delete path from the spec, and an
    /// undocumented deletion API is not a usable deletion API.
    #[tokio::test]
    async fn openapi_documents_the_delete_purge_flag() {
        let app = build_router(sample_state());
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

        let params = spec["paths"]["/sessions/{id}"]["delete"]["parameters"]
            .as_array()
            .expect("delete must declare parameters");
        let purge = params
            .iter()
            .find(|p| p["name"] == "purge")
            .expect("delete must document the purge query flag");
        assert_eq!(purge["in"], "query");
        assert_eq!(purge["schema"]["type"], "boolean");
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
            "recursive_runs_waiting",
            "recursive_runs_in_flight",
            "recursive_transcript_bytes_total",
            // Issue #113: labelled families and histograms.
            "recursive_agent_runs_finished_total",
            "recursive_cost_usd_total",
            "recursive_tool_errors_total",
            "recursive_llm_retries_total",
            "recursive_compactions_total",
            "recursive_compaction_skipped_total",
            "recursive_llm_latency_ms",
            "recursive_run_steps",
            "recursive_admission_wait_ms",
        ] {
            assert!(text.contains(name), "missing metric: {name}");
        }

        // NB: the scrape request itself is counted *after* this handler
        // returns (the middleware records the status once the response
        // exists), so the labelled `recursive_requests_total` series appear
        // from the second scrape onwards — see
        // `metrics_middleware_increments_requests_total`.
    }

    #[tokio::test]
    async fn metrics_middleware_increments_requests_total() {
        let state = sample_state();
        let metrics = state.metrics.clone();
        let app = build_router(state);

        // Hit two non-/metrics endpoints to drive the middleware, plus an
        // unmatched path (route label fallback) and a 4xx (status label).
        for uri in [
            "/health",
            "/tools",
            "/definitely-not-a-route",
            "/sessions/does-not-exist",
        ] {
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

        // Issue #113: requests are counted per matched route + status.
        let n = metrics.requests_by_route.total();
        assert!(n >= 2, "expected requests_total >= 2, got {n}");
        let routes: Vec<String> = metrics
            .requests_by_route
            .snapshot()
            .into_iter()
            .map(|(labels, _)| labels[0].clone())
            .collect();
        assert!(
            routes.contains(&"/health".to_string()) && routes.contains(&"/tools".to_string()),
            "route label must be the matched route template, got {routes:?}"
        );
        let statuses: Vec<String> = metrics
            .requests_by_route
            .snapshot()
            .into_iter()
            .map(|(labels, _)| labels[1].clone())
            .collect();
        assert!(
            statuses.contains(&"200".to_string()),
            "the successful probes must be labelled 200, got {statuses:?}"
        );
        assert!(
            statuses.contains(&"404".to_string()),
            "the failed lookups must be labelled with their status: {statuses:?}"
        );
        assert!(
            routes.contains(&"unmatched".to_string()),
            "a path no route matched must fall back to the `unmatched` label: {routes:?}"
        );

        // Issue #113: the exposition renders the labelled series, so a
        // per-route 5xx rate is computable from a scrape.
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
        assert!(
            text.contains("recursive_requests_total{route=\"/health\",status=\"200\"} 1"),
            "missing labelled request series: {text}"
        );
        assert!(
            text.contains("recursive_requests_total{route=\"/tools\",status=\"200\"} 1"),
            "missing labelled request series: {text}"
        );
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
        // sample_state() sets INSECURE_OK=1, so the request passes through.
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
        // Even with auth enabled, /health, /healthz, /readyz and /metrics must
        // answer unauthenticated (k8s liveness/readiness + Prometheus).
        let auth = AuthConfig::new(vec!["secret".into()]);
        let app = build_router_with_auth(sample_state(), auth);

        for uri in ["/health", "/healthz", "/readyz", "/metrics"] {
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
        // Empty config: is_valid always returns false (no keys to match).
        // Auth bypass for the "disabled" case is handled by is_enabled() +
        // auth_middleware, not by is_valid() returning true.
        let empty = AuthConfig::default();
        assert!(!empty.is_valid(""));
        assert!(!empty.is_valid("anything"));
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

        for uri in ["/health", "/healthz", "/readyz", "/metrics"] {
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

    // ── Issue #85: identity & session ownership ───────────────────────────

    /// Two API keys attributed to two distinct callers.
    fn two_caller_auth() -> AuthConfig {
        AuthConfig::new(vec!["key-a".into(), "key-b".into()])
            .with_key_subject("key-a", "alice")
            .with_key_subject("key-b", "bob")
    }

    fn api_request(method: &str, uri: &str, key: &str, body: &str) -> axum::http::Request<Body> {
        axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("X-API-Key", key)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn bearer_request(
        method: &str,
        uri: &str,
        token: &str,
        body: &str,
    ) -> axum::http::Request<Body> {
        axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("Authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    async fn status(app: &axum::Router, request: axum::http::Request<Body>) -> u16 {
        app.clone()
            .oneshot(request)
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    /// POST /sessions with an authenticated request, returning the new id.
    async fn created_session_id(app: &axum::Router, request: axum::http::Request<Body>) -> String {
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), 201, "session creation must succeed");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        json["id"].as_str().expect("session id").to_string()
    }

    fn mint_token_with(secret: &str, claims: serde_json::Value) -> String {
        use jsonwebtoken::{encode, EncodingKey, Header};
        encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .expect("mint jwt")
    }

    /// Issue #85: every `/sessions/:id*` route mutates or reads one caller's
    /// data, so a valid credential must not reach another caller's session —
    /// before the fix any keyholder could read, mutate and delete all of them.
    #[tokio::test]
    async fn sessions_are_scoped_to_their_owner() {
        // A generous limiter: this test drives a dozen requests on one key and
        // is about authorization, not rate limiting.
        let app = build_router_with_auth_and_rate_limit(
            sample_state(),
            two_caller_auth(),
            RateLimiter::new(10_000, 10_000.0),
        );
        let alice_session =
            created_session_id(&app, api_request("POST", "/sessions", "key-a", "{}")).await;

        // Bob's list is empty; alice's holds her session.
        let response = app
            .clone()
            .oneshot(api_request("GET", "/sessions", "key-b", "{}"))
            .await
            .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let list: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(list["total"], 0, "bob must not see alice's sessions");
        assert_eq!(
            status(&app, api_request("GET", "/sessions", "key-a", "{}")).await,
            200
        );

        // Every per-session route refuses bob…
        for (method, uri, body) in [
            (
                "GET",
                format!("/sessions/{alice_session}"),
                "{}".to_string(),
            ),
            (
                "PATCH",
                format!("/sessions/{alice_session}"),
                r#"{"title":"hijacked"}"#.to_string(),
            ),
            (
                "DELETE",
                format!("/sessions/{alice_session}"),
                "{}".to_string(),
            ),
            (
                "POST",
                format!("/sessions/{alice_session}/messages"),
                r#"{"content":"hi"}"#.to_string(),
            ),
            (
                "POST",
                format!("/sessions/{alice_session}/plan/confirm"),
                "{}".to_string(),
            ),
            (
                "POST",
                format!("/sessions/{alice_session}/plan/reject"),
                "{}".to_string(),
            ),
            (
                "POST",
                format!("/sessions/{alice_session}/goal"),
                r#"{"condition":"done"}"#.to_string(),
            ),
            (
                "DELETE",
                format!("/sessions/{alice_session}/goal"),
                "{}".to_string(),
            ),
            (
                "POST",
                format!("/sessions/{alice_session}/interrupt"),
                "{}".to_string(),
            ),
            (
                "POST",
                format!("/sessions/{alice_session}/fork"),
                "{}".to_string(),
            ),
            (
                "GET",
                format!("/sessions/{alice_session}/events"),
                "{}".to_string(),
            ),
        ] {
            assert_eq!(
                status(&app, api_request(method, &uri, "key-b", &body)).await,
                403,
                "{method} {uri} must refuse another caller's session"
            );
        }

        // …while the owner still reaches it, and bob's refusals changed
        // nothing (the failed DELETE in particular).
        assert_eq!(
            status(
                &app,
                api_request("GET", &format!("/sessions/{alice_session}"), "key-a", "{}")
            )
            .await,
            200
        );
        let response = app
            .oneshot(api_request("GET", "/sessions", "key-a", "{}"))
            .await
            .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let list: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(list["total"], 1, "the refused DELETE must not have run");
        assert!(
            list["sessions"][0]["title"].is_null(),
            "the refused PATCH must not have run"
        );
    }

    /// Issue #85: the admin role is the configured escape hatch — an operator
    /// (or a migration tool) needs to reach sessions it did not create.
    #[tokio::test]
    async fn admin_identity_reaches_other_callers_sessions() {
        let app = build_router_with_auth(sample_state(), two_caller_auth().with_admin("bob"));
        let alice_session =
            created_session_id(&app, api_request("POST", "/sessions", "key-a", "{}")).await;

        assert_eq!(
            status(
                &app,
                api_request("GET", &format!("/sessions/{alice_session}"), "key-b", "{}")
            )
            .await,
            200,
            "an admin identity reaches a foreign session"
        );
        let response = app
            .oneshot(api_request("GET", "/sessions", "key-b", "{}"))
            .await
            .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let list: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(list["total"], 1, "an admin sees every session");
    }

    /// Issue #85: JWT callers are scoped by `sub`, exactly like API-key
    /// callers are scoped by their configured subject.
    #[tokio::test]
    async fn jwt_sub_scopes_sessions() {
        let secret = "test-secret-identity";
        let jwt = JwtConfig::hs256(secret, None).unwrap();
        let app = build_router_with_auth(sample_state(), AuthConfig::new(Vec::new()).with_jwt(jwt));
        let alice = mint_token_with(
            secret,
            serde_json::json!({"exp": now_secs() + 60, "sub": "alice"}),
        );
        let bob = mint_token_with(
            secret,
            serde_json::json!({"exp": now_secs() + 60, "sub": "bob"}),
        );

        let alice_session =
            created_session_id(&app, bearer_request("POST", "/sessions", &alice, "{}")).await;

        assert_eq!(
            status(
                &app,
                bearer_request("GET", &format!("/sessions/{alice_session}"), &bob, "{}")
            )
            .await,
            403,
            "another `sub` must not read the session"
        );
        assert_eq!(
            status(
                &app,
                bearer_request("GET", &format!("/sessions/{alice_session}"), &alice, "{}")
            )
            .await,
            200,
            "the owning `sub` keeps access"
        );
        let response = app
            .oneshot(bearer_request("GET", "/sessions", &bob, "{}"))
            .await
            .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let list: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            list["total"], 0,
            "bob's list must not contain alice's session"
        );
    }

    /// Issue #85: a token without `sub` cannot be attributed to an owner, so
    /// it is not an identity — 401 rather than a shared anonymous principal.
    #[tokio::test]
    async fn jwt_without_sub_is_rejected() {
        let secret = "test-secret-attribution";
        let jwt = JwtConfig::hs256(secret, None).unwrap();
        let app = build_router_with_auth(sample_state(), AuthConfig::new(Vec::new()).with_jwt(jwt));
        let anonymous = mint_token_with(secret, serde_json::json!({"exp": now_secs() + 60}));

        assert_eq!(
            status(&app, bearer_request("GET", "/sessions", &anonymous, "{}")).await,
            401,
            "a valid but unattributable token must not be accepted"
        );
    }

    /// Issue #85: a session written before the identity model (or restored
    /// from an older build's metadata) belongs to nobody — it is reachable by
    /// admins only, never by every caller.
    #[tokio::test]
    async fn unattributed_sessions_are_admin_only() {
        let state = sample_state();
        let runtime = AgentRuntimeBuilder::new()
            .llm(Arc::new(MockProvider::new(vec![])))
            .build()
            .expect("runtime build failed");
        let gate = runtime.plan_approval_gate();
        state.host.sessions().write().await.insert(
            "legacy-session".to_string(),
            SessionState {
                id: "legacy-session".to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                title: None,
                owner: None,
                tenant: None,
                runtime: Arc::new(tokio::sync::Mutex::new(runtime)),
                plan_approval_gate: gate,
                interrupt_token: Arc::new(tokio::sync::Mutex::new(None)),
                non_system_message_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                last_active_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                usage: Arc::new(SessionUsage::new("test-model")),
                event_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                event_log: Arc::new(recursive::http::SessionEventLog::new(
                    recursive::http::SESSION_EVENT_LOG_CAPACITY,
                )),
            },
        );

        let app = build_router_with_auth(state.clone(), two_caller_auth());
        assert_eq!(
            status(
                &app,
                api_request("GET", "/sessions/legacy-session", "key-a", "{}")
            )
            .await,
            403,
            "an unattributed session must not be everyone's"
        );
        let response = app
            .oneshot(api_request("GET", "/sessions", "key-a", "{}"))
            .await
            .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let list: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(list["total"], 0, "nor appear in a caller's list");

        let admin_app = build_router_with_auth(state, two_caller_auth().with_admin("alice"));
        assert_eq!(
            status(
                &admin_app,
                api_request("GET", "/sessions/legacy-session", "key-a", "{}")
            )
            .await,
            200,
            "an admin can still reach a pre-identity session"
        );
    }

    /// Issue #85: a fork is a session like any other, so its ownership (and the
    /// preset it inherited, issue #127) must survive a restart. It used to
    /// write no metadata blob, so a cold load found no owner: the creator's own
    /// fork answered 403, vanished from `GET /sessions` and was no longer
    /// deletable over the API.
    #[tokio::test]
    async fn fork_ownership_survives_a_restart() {
        let dir = tempfile::tempdir().expect("storage tempdir");
        let backend = Arc::new(recursive::storage::LocalStorageBackend::new(
            dir.path().to_path_buf(),
        ));
        let state = sample_state_with_storage(
            Arc::new(MockProvider::new(vec![Completion {
                content: "hello".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            }])),
            backend.clone(),
        );
        let app = build_router_with_auth(state.clone(), two_caller_auth());

        let source =
            created_session_id(&app, api_request("POST", "/sessions", "key-a", "{}")).await;
        // One real turn, so the fork's copied transcript is restorable.
        assert_eq!(
            status(
                &app,
                api_request(
                    "POST",
                    &format!("/sessions/{source}/messages"),
                    "key-a",
                    r#"{"content":"hi"}"#,
                )
            )
            .await,
            200
        );
        let fork = created_session_id(
            &app,
            api_request("POST", &format!("/sessions/{source}/fork"), "key-a", "{}"),
        )
        .await;

        // Graceful shutdown: the next process sees only storage.
        recursive::http::flush_all_sessions(&state).await;

        let restarted =
            sample_state_with_storage(Arc::new(MockProvider::new(vec![])), backend.clone());
        let app2 = build_router_with_auth(restarted, two_caller_auth());

        let response = app2
            .clone()
            .oneshot(api_request(
                "GET",
                &format!("/sessions/{fork}"),
                "key-a",
                "{}",
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            200,
            "the fork's creator must keep access after a restart"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let detail: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            detail["preset"], "standard",
            "the inherited preset must survive the restart too"
        );

        assert_eq!(
            status(
                &app2,
                api_request("GET", &format!("/sessions/{fork}"), "key-b", "{}")
            )
            .await,
            403,
            "another caller must not inherit access to the fork"
        );
        let response = app2
            .clone()
            .oneshot(api_request("GET", "/sessions", "key-a", "{}"))
            .await
            .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let list: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // The list is memory-only (cold load is lazy), so the just-restored
        // fork is the single entry.
        assert_eq!(
            list["total"], 1,
            "the restored fork must reappear in its creator's list"
        );
        assert_eq!(list["sessions"][0]["id"], fork.as_str());

        // Reachable means manageable: the restored fork is still deletable by
        // its creator, and still refused to everyone else.
        assert_eq!(
            status(
                &app2,
                api_request("DELETE", &format!("/sessions/{fork}"), "key-b", "{}")
            )
            .await,
            403
        );
        assert_eq!(
            status(
                &app2,
                api_request("DELETE", &format!("/sessions/{fork}"), "key-a", "{}")
            )
            .await,
            204,
            "the creator must be able to delete its own restored fork"
        );
    }

    /// Issue #121: a session that closes over HTTP is mirrored into the native
    /// session layout, so `recursive sessions list` and the resume picker see
    /// it. The root is injected on the `AppState` (not read from the
    /// environment), which is what lets this assertion run hermetically
    /// alongside every other test in this binary.
    #[tokio::test]
    async fn graceful_shutdown_mirrors_the_session_into_the_native_layout() {
        let mirror_root = tempfile::tempdir().expect("mirror root");
        let dir = tempfile::tempdir().expect("storage tempdir");
        let backend = Arc::new(recursive::storage::LocalStorageBackend::new(
            dir.path().to_path_buf(),
        ));
        let mut state = sample_state_with_storage(
            Arc::new(MockProvider::new(vec![Completion {
                content: "hello".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            }])),
            backend,
        );
        state.session_mirror_root = Some(mirror_root.path().to_path_buf());
        let app = build_router_with_auth(state.clone(), two_caller_auth());

        let id = created_session_id(&app, api_request("POST", "/sessions", "key-a", "{}")).await;
        assert_eq!(
            status(
                &app,
                api_request(
                    "POST",
                    &format!("/sessions/{id}/messages"),
                    "key-a",
                    r#"{"content":"hi"}"#,
                )
            )
            .await,
            200
        );

        recursive::http::flush_all_sessions(&state).await;

        // Only the injected root was written to, and the session is there in
        // the shape a CLI reader consumes.
        let slug_dir = std::fs::read_dir(mirror_root.path())
            .expect("mirror root")
            .map(|e| e.expect("entry").path())
            .find(|p| p.join(&id).is_dir())
            .expect("the mirrored session must exist under the injected root");
        let session_dir = slug_dir.join(&id);
        let meta = recursive::session::SessionReader::load_meta(&session_dir).expect("mirror meta");
        assert_eq!(meta.session_id, id);
        assert_eq!(meta.goal, "hi", "the mirror carries the session's prompt");
        let entries = recursive::session::SessionReader::load_full_history(&session_dir)
            .expect("mirror transcript");
        assert!(
            !entries.is_empty(),
            "the mirrored transcript must not be empty"
        );
    }

    /// Issue #85: a trigger that resumes a session runs turns in it
    /// server-side (as an admin identity), so registration is ownership-
    /// asserted too — otherwise the `/sessions` scoping would be cosmetic.
    #[tokio::test]
    async fn trigger_registration_cannot_target_a_foreign_session() {
        use crate::trigger_endpoints::state_with_workspace;
        let ws = tempfile::tempdir().expect("workspace tempdir");
        let state = state_with_workspace(ws.path());
        let app = build_router_with_auth(state, two_caller_auth());
        let alice_session =
            created_session_id(&app, api_request("POST", "/sessions", "key-a", "{}")).await;
        let body =
            format!(r#"{{"kind":"webhook","goal":"do something","session_id":"{alice_session}"}}"#);

        assert_eq!(
            status(&app, api_request("POST", "/triggers", "key-b", &body)).await,
            403,
            "a trigger must not be pointed at another caller's session"
        );
        assert_eq!(
            status(&app, api_request("POST", "/triggers", "key-a", &body)).await,
            201,
            "the owner may resume its own session on a schedule"
        );
    }

    /// Issue #85: the trigger registry is workspace-global and a trigger runs
    /// its goal server-side (in its session, as an admin identity), so every
    /// route that reads or mutates one is owner-scoped too — otherwise any
    /// caller could re-goal another caller's trigger and have their work run
    /// in that caller's session, which is exactly the gap `GET /sessions`
    /// scoping leaves open.
    #[tokio::test]
    async fn triggers_are_scoped_to_their_owner() {
        use crate::trigger_endpoints::state_with_workspace;
        let ws = tempfile::tempdir().expect("workspace tempdir");
        let state = state_with_workspace(ws.path());
        let app = build_router_with_auth(state, two_caller_auth());

        let response = app
            .clone()
            .oneshot(api_request(
                "POST",
                "/triggers",
                "key-a",
                r#"{"kind":"webhook","goal":"alice's job"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), 201);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let id = created["id"].as_str().expect("trigger id").to_string();
        let keyed_path = created["webhook_path"]
            .as_str()
            .expect("webhook path")
            .to_string();

        // Bob's list does not contain alice's trigger…
        let response = app
            .clone()
            .oneshot(api_request("GET", "/triggers", "key-b", "{}"))
            .await
            .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let list: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            list.as_array().unwrap().is_empty(),
            "a caller must not see another caller's triggers"
        );

        // …and every route that reaches one refuses him, the fire path
        // included (knowing the id — and even the webhook secret — is not
        // authorization).
        for (method, uri, payload) in [
            ("GET", format!("/triggers/{id}"), "{}".to_string()),
            (
                "PATCH",
                format!("/triggers/{id}"),
                r#"{"goal":"bob's job","enabled":true}"#.to_string(),
            ),
            ("DELETE", format!("/triggers/{id}"), "{}".to_string()),
            ("POST", keyed_path.clone(), "{}".to_string()),
        ] {
            assert_eq!(
                status(&app, api_request(method, &uri, "key-b", &payload)).await,
                403,
                "{method} {uri} must refuse another caller's trigger"
            );
        }

        // The refused PATCH/DELETE changed nothing…
        let response = app
            .clone()
            .oneshot(api_request(
                "GET",
                &format!("/triggers/{id}"),
                "key-a",
                "{}",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let detail: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(detail["goal"], "alice's job");
        assert_eq!(detail["enabled"], false, "the refused PATCH must not run");
        let response = app
            .clone()
            .oneshot(api_request("GET", "/triggers", "key-a", "{}"))
            .await
            .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let list: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(list.as_array().unwrap().len(), 1);

        // …while the owner still manages it.
        assert_eq!(
            status(
                &app,
                api_request(
                    "PATCH",
                    &format!("/triggers/{id}"),
                    "key-a",
                    r#"{"goal":"alice v2"}"#
                )
            )
            .await,
            200
        );
        assert_eq!(
            status(
                &app,
                api_request("DELETE", &format!("/triggers/{id}"), "key-a", "{}")
            )
            .await,
            204
        );
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

    /// Keep the `/agui` tests out of the developer's real sessions store.
    ///
    /// `/agui` resolves a thread's directory through `paths::user_sessions_dir`,
    /// which has no per-`AppState` injection point — so unlike the mirror
    /// fixtures (see `http_common`) these tests would otherwise write a session
    /// into `~/.recursive/.../sessions/<workspace-slug>/` on every run. Pin the
    /// root once per process: `RECURSIVE_SESSIONS_DIR` is a hard override read
    /// by `user_sessions_dir` alone, and the value never changes afterwards, so
    /// no test can observe a different root depending on scheduling.
    fn pin_agui_sessions_root() {
        static ROOT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        ROOT.get_or_init(|| {
            // Leaked on purpose: the root must outlive every test in the process.
            let dir: &'static tempfile::TempDir =
                Box::leak(Box::new(tempfile::tempdir().expect("agui sessions root")));
            // SAFETY: written exactly once per test process, before this helper
            // returns to any caller, and every reader in this binary wants the
            // throwaway root.
            unsafe { std::env::set_var("RECURSIVE_SESSIONS_DIR", dir.path()) };
        });
    }

    /// Build an `AppState` for an `/agui` test on a throwaway workspace.
    ///
    /// Issue #147: a run resolves the thread's session directory through
    /// `config.workspace`, and wires per-turn checkpoints against it too (a
    /// shadow `git add -A`). The shared fixtures point that at `/tmp`, so a
    /// thread the process has not snapshotted before pays a full walk of the
    /// real `/tmp` on the failure path (tens of seconds), and every test lands
    /// its session in one shared workspace subtree. A tempdir per test keeps
    /// the snapshot to the test's own files — and gives each test a workspace
    /// slug of its own, so a leftover thread from an earlier run cannot leak in.
    fn agui_state(provider: Arc<MockProvider>) -> (tempfile::TempDir, AppState) {
        pin_agui_sessions_root();
        let ws = tempfile::tempdir().expect("agui workspace");
        let mut state = sample_state_with_provider(provider);
        state.config.workspace = ws.path().to_path_buf();
        (ws, state)
    }

    /// The thread id is explicit on purpose: issue #147 seeds a run from the
    /// thread's *persisted* transcript, so two tests sharing an id would read
    /// each other's conversation.
    fn agui_request_body(
        thread: &str,
        messages: serde_json::Value,
        context: serde_json::Value,
    ) -> String {
        serde_json::to_string(&serde_json::json!({
            "threadId": thread,
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
        let (_ws, state) = agui_state(provider);
        let app = build_router(state);

        let body = agui_request_body(
            "t-streams",
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
                assert_eq!(rs.thread_id, "t-streams");
                assert_eq!(rs.run_id, "r-test");
            }
            other => panic!("expected RunStarted first, got {other:?}"),
        }

        // Last event must be RunFinished.
        match events.last().unwrap() {
            agui_protocol::Event::RunFinished(rf) => {
                assert_eq!(rf.thread_id, "t-streams");
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

    /// Issue #147: empty `messages` is legal for a thread the server has a
    /// persisted transcript for — naming the thread is enough to continue
    /// (covered end to end in `tests/agui_e2e.rs`). A thread the server has
    /// never seen still needs its prompt, so that request is the bad request it
    /// always was.
    #[tokio::test]
    async fn agui_endpoint_rejects_empty_messages_for_an_unknown_thread() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let (_ws, state) = agui_state(provider);
        let app = build_router(state);

        let body = agui_request_body(
            "t-empty-unknown",
            serde_json::json!([]),
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
        let (_ws, state) = agui_state(provider);
        let app = build_router(state);

        let body = agui_request_body(
            "t-tool-calls",
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

    /// Goal 359: when `runtime.run()` returns `Err` (LLM failure, provider
    /// down, ...), the AG-UI RunFinished must carry an `Error` outcome —
    /// NOT a silent `Success` with `result: None`. This test locks the
    /// driver's else-branch fix: it configures a MockProvider whose first
    /// `complete()` returns `Error::Llm`, drives the real /agui endpoint,
    /// and asserts the terminal event reports the failure.
    #[tokio::test]
    async fn agui_runfinished_reports_error_when_run_fails() {
        use recursive::error::Error;
        let provider = Arc::new(MockProvider::new(vec![]).with_errors(vec![Error::Llm {
            provider: "mock".into(),
            model: None,
            request_id: None,
            message: "injected provider failure".into(),
        }]));
        let (_ws, state) = agui_state(provider);
        let app = build_router(state);

        let body = agui_request_body(
            "t-run-failed",
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
        let events = collect_agui_events(response).await;
        assert!(!events.is_empty(), "expected at least one AG-UI event");

        // First event must still be RunStarted.
        match &events[0] {
            agui_protocol::Event::RunStarted(_) => {}
            other => panic!("expected RunStarted first, got {other:?}"),
        }

        // Last event must be RunFinished carrying an Error outcome.
        match events.last().unwrap() {
            agui_protocol::Event::RunFinished(rf) => {
                let outcome = rf
                    .outcome
                    .as_ref()
                    .expect("RunFinished must carry an outcome");
                match outcome {
                    agui_protocol::RunFinishedOutcome::Error { message, code } => {
                        assert!(
                            message.contains("injected provider failure"),
                            "message should surface the underlying error, got: {message}"
                        );
                        assert!(code.is_none(), "code is reserved for future mapping");
                    }
                    other => panic!("expected Error outcome, got {other:?}"),
                }
            }
            other => panic!("expected RunFinished last, got {other:?}"),
        }
    }

    /// Issue #114: an `/agui` run is a completed run, so its USD must reach the
    /// same global counter as `/run` and the session path — otherwise the
    /// "across completed runs" HELP text quietly excludes AG-UI spend.
    #[tokio::test]
    async fn agui_run_feeds_the_global_cost_counter() {
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "hello from mock".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: Some(recursive::llm::TokenUsage {
                prompt_tokens: 1_000_000,
                completion_tokens: 500_000,
                total_tokens: 1_500_000,
                ..Default::default()
            }),
            reasoning_content: None,
        }]));
        let (_ws, mut state) = agui_state(provider);
        state.config.model = "deepseek-chat".into();
        let metrics = state.metrics.clone();
        let app = build_router(state);

        let body = agui_request_body(
            "t-cost",
            serde_json::json!([{"id": "u1", "role": "user", "content": "say hello"}]),
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
        assert!(
            matches!(events.last(), Some(agui_protocol::Event::RunFinished(_))),
            "the run must finish before its metrics are asserted"
        );

        assert_eq!(
            metrics
                .agent_runs_total
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert!(
            metrics.cost_usd_total() > 0.0,
            "an AG-UI run must feed recursive_cost_usd_total too"
        );
    }

    // ── Issue #114: session usage / cost over HTTP ────────────────────────

    /// GET `uri` with `key` and parse the JSON body, asserting 200.
    async fn get_json(app: &axum::Router, uri: &str, key: &str) -> serde_json::Value {
        let response = app
            .clone()
            .oneshot(api_request("GET", uri, key, "{}"))
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "GET {uri} must succeed");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&body).unwrap()
    }

    fn priced_completion() -> Completion {
        Completion {
            content: "hello".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: Some(recursive::llm::TokenUsage {
                reasoning_tokens: 0,
                prompt_tokens: 1_000_000,
                completion_tokens: 500_000,
                total_tokens: 1_500_000,
                cache_hit_tokens: 600_000,
                cache_miss_tokens: 400_000,
            }),
            reasoning_content: None,
        }
    }

    /// Issue #114 acceptance: `GET /sessions/:id/usage` reports the cache hit
    /// / miss split and USD, and (unlike before) the numbers survive a server
    /// restart — the accumulator is persisted after every turn and restored by
    /// the cold-load path.
    #[tokio::test]
    async fn session_usage_reports_and_survives_a_restart() {
        let dir = tempfile::tempdir().expect("storage tempdir");
        let backend = Arc::new(recursive::storage::LocalStorageBackend::new(
            dir.path().to_path_buf(),
        ));
        let mut state = sample_state_with_storage(
            Arc::new(MockProvider::new(vec![priced_completion()])),
            backend.clone(),
        );
        // A priced model so `cost_usd` is a number, not null.
        state.config.model = "deepseek-chat".into();
        let app = build_router_with_auth(state.clone(), two_caller_auth());

        let sid = created_session_id(&app, api_request("POST", "/sessions", "key-a", "{}")).await;
        assert_eq!(
            status(
                &app,
                api_request(
                    "POST",
                    &format!("/sessions/{sid}/messages"),
                    "key-a",
                    r#"{"content":"hi"}"#
                )
            )
            .await,
            200
        );

        let usage = get_json(&app, &format!("/sessions/{sid}/usage"), "key-a").await;
        assert_eq!(usage["prompt_tokens"], 1_000_000);
        assert_eq!(usage["completion_tokens"], 500_000);
        assert_eq!(usage["cache_hit_tokens"], 600_000);
        assert_eq!(usage["cache_miss_tokens"], 400_000);
        assert_eq!(usage["total_tokens"], 1_500_000);
        assert_eq!(usage["model"], "deepseek-chat");
        let live_cost = usage["cost_usd"].as_f64().expect("priced cost");
        assert!(live_cost > 0.0, "priced session must report USD");

        // Graceful shutdown: the next process sees only storage.
        recursive::http::flush_all_sessions(&state).await;
        let mut restarted =
            sample_state_with_storage(Arc::new(MockProvider::new(vec![])), backend.clone());
        restarted.config.model = "deepseek-chat".into();
        let app2 = build_router_with_auth(restarted, two_caller_auth());

        let restored = get_json(&app2, &format!("/sessions/{sid}/usage"), "key-a").await;
        assert_eq!(
            restored["prompt_tokens"], 1_000_000,
            "usage must survive a restart (was zeroed before issue #114)"
        );
        assert_eq!(restored["completion_tokens"], 500_000);
        assert_eq!(restored["cache_hit_tokens"], 600_000);
        assert_eq!(restored["cache_miss_tokens"], 400_000);
        assert_eq!(restored["model"], "deepseek-chat");
        assert_eq!(restored["cost_usd"].as_f64(), Some(live_cost));
    }

    /// The usage endpoint honours the same ownership contract as every other
    /// `/sessions/:id*` route, and an unknown id stays a 404.
    #[tokio::test]
    async fn session_usage_is_owner_scoped() {
        let state = sample_state_with_provider(Arc::new(MockProvider::new(vec![])));
        let app = build_router_with_auth(state, two_caller_auth());
        let sid = created_session_id(&app, api_request("POST", "/sessions", "key-a", "{}")).await;

        assert_eq!(
            status(
                &app,
                api_request("GET", &format!("/sessions/{sid}/usage"), "key-b", "{}")
            )
            .await,
            403,
            "another caller must not read the usage"
        );
        assert_eq!(
            status(
                &app,
                api_request("GET", "/sessions/does-not-exist/usage", "key-a", "{}")
            )
            .await,
            404
        );
    }

    /// Issue #114: the one-shot `POST /run` response carries the cache split
    /// and USD too, so a caller can bill without a second request.
    #[tokio::test]
    async fn run_response_reports_cache_split_and_cost() {
        let mut state =
            sample_state_with_provider(Arc::new(MockProvider::new(vec![priced_completion()])));
        state.config.model = "deepseek-chat".into();
        let metrics = state.metrics.clone();
        let app = build_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/run")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"goal":"hi"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let resp: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(resp["usage"]["total_tokens"], 1_500_000);
        assert_eq!(resp["usage"]["prompt_tokens"], 1_000_000);
        assert_eq!(resp["usage"]["completion_tokens"], 500_000);
        assert_eq!(resp["usage"]["cache_hit_tokens"], 600_000);
        assert_eq!(resp["usage"]["cache_miss_tokens"], 400_000);
        assert_eq!(resp["usage"]["model"], "deepseek-chat");
        let cost = resp["usage"]["cost_usd"].as_f64().expect("priced cost");
        assert!(cost > 0.0, "priced run must report USD");
        // The global USD counter moved with the run.
        assert!(metrics.cost_usd_total() > 0.0);
    }

    /// Issue #115 fixed the global books for a failed turn; the session ledger
    /// must not silently drop the same spend.
    #[tokio::test]
    async fn session_usage_keeps_a_failed_turns_spend() {
        // Step 1 succeeds with a tool call carrying usage; step 2's LLM call
        // finds the scripted queue empty and fails — the runtime stashes the
        // completed step's tokens in `last_failed_usage`.
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: String::new(),
            tool_calls: vec![recursive::llm::ToolCall {
                id: "call_1".into(),
                name: "nonexistent_tool".into(),
                arguments: serde_json::json!({}),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: Some(recursive::llm::TokenUsage {
                prompt_tokens: 77,
                completion_tokens: 21,
                total_tokens: 98,
                ..Default::default()
            }),
            reasoning_content: None,
        }]));
        let state = sample_state_with_provider(provider);
        let app = build_router_with_auth(state, two_caller_auth());
        let sid = created_session_id(&app, api_request("POST", "/sessions", "key-a", "{}")).await;

        let code = status(
            &app,
            api_request(
                "POST",
                &format!("/sessions/{sid}/messages"),
                "key-a",
                r#"{"content":"go"}"#,
            ),
        )
        .await;
        assert_eq!(code, 500, "the failing turn must surface as an error");

        let usage = get_json(&app, &format!("/sessions/{sid}/usage"), "key-a").await;
        assert_eq!(
            usage["prompt_tokens"], 77,
            "a failed turn still spent its completed steps' tokens"
        );
        assert_eq!(usage["completion_tokens"], 21);
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
            owner: None,
            tenant: None,
            runtime: Arc::new(tokio::sync::Mutex::new(runtime)),
            plan_approval_gate: gate,
            interrupt_token: Arc::new(tokio::sync::Mutex::new(None)),
            non_system_message_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            last_active_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            usage: Arc::new(SessionUsage::new("test-model")),
            event_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            event_log: Arc::new(recursive::http::SessionEventLog::new(
                recursive::http::SESSION_EVENT_LOG_CAPACITY,
            )),
        };
        state
            .host
            .sessions()
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
                .host
                .sessions()
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
        // The handler reads pending_plan (with edits applied) and then calls
        // approve(), which clears pending_plan to prevent stale re-injection.
        let after = gate.pending_plan.read().unwrap().clone();
        assert!(
            after.is_none(),
            "pending_plan must be cleared after approve() to prevent stale re-injection"
        );
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
            host: std::sync::Arc::new(recursive::session_host::SessionHost::new(
                std::time::Duration::from_secs(0),
                recursive::http::AdmissionGate::new(
                    8,
                    std::time::Duration::ZERO,
                    std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                ),
            )),
            rate_limiter: RateLimiter::new(10, 1.0),
            skills: vec![],
            storage: Arc::new(recursive::storage::LocalStorageBackend::new(
                std::env::temp_dir().join(format!("recursive-http-test-{}", std::process::id())),
            )),
            agui_active_runs: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            // Issue #121: no native session mirror for these fixtures.
            session_mirror_root: None,
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

    // ── #74 拆单 3/3: /skills endpoint (service-level skill sources) ─────────

    #[tokio::test]
    async fn skills_endpoint_reports_source_content_vs_filesystem() {
        // Content-backed skill = the shape a service-level SkillSource
        // delivers (`skill_from_content`: in-memory body, /virtual path) —
        // the "never lands on disk" contract surfaced as `source: "content"`.
        let content_backed = recursive::skills::skill_from_content(
            "remote-skill",
            "---\nname: remote-skill\ndescription: from the wire\nmode: trigger\ntriggers: deploy\n---\n\nDeploy checklist.",
            Vec::new(),
        );
        // Filesystem-backed skill = directory discovery (body: None).
        let fs_backed = recursive::skills::Skill {
            name: "local-skill".to_string(),
            description: "discovered on disk".to_string(),
            path: std::path::PathBuf::from("/tmp/skills/local-skill/SKILL.md"),
            mode: recursive::skills::SkillMode::Manual,
            triggers: vec![],
            hint: String::new(),
            depends_on: vec![],
            refs: vec![],
            params: vec![],
            scripts: vec![],
            sections: vec![],
            globs: None,
            body: None,
        };
        let mut state = sample_state();
        state.skills = vec![content_backed, fs_backed];
        let app = build_router(state);

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/skills")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let skills: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let arr = skills.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        let remote = arr
            .iter()
            .find(|s| s["name"] == "remote-skill")
            .expect("content-backed skill must be listed");
        assert_eq!(remote["source"], "content");
        assert_eq!(remote["mode"], "trigger");
        assert_eq!(remote["description"], "from the wire");
        let local = arr
            .iter()
            .find(|s| s["name"] == "local-skill")
            .expect("filesystem skill must be listed");
        assert_eq!(local["source"], "filesystem");
        assert_eq!(local["mode"], "manual");
    }

    #[tokio::test]
    async fn skills_endpoint_returns_empty_array_without_skills() {
        let app = build_router(sample_state());
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/skills")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let skills: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(skills.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn skills_endpoint_lists_skills_in_openapi_spec() {
        let app = build_router(sample_state());
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
        assert!(
            spec["paths"]["/skills"]["get"].is_object(),
            "/skills must be documented in the OpenAPI spec"
        );
        assert!(
            spec["components"]["schemas"]["SkillInfo"].is_object(),
            "SkillInfo schema must be present"
        );
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

    // ── Goal-306: fork_session.message_count must match the non-system
    //    semantics of `SessionInfo.message_count` from `GET /sessions`,
    //    not the raw `transcript.len()` (which would include the system
    //    prompt). Sending one user message produces 2 non-system
    //    messages (user + assistant) on top of the system prompt, so the
    //    transcript has length 3 but the count returned by fork should
    //    be 2. Before the fix the response returned 3.

    /// Regression: `POST /sessions/:id/fork` returns `message_count` equal
    /// to the **non-system** message count, matching
    /// `SessionInfo.message_count` from `GET /sessions`. Previously the
    /// handler used `transcript_snapshot.len()` which inflated the value
    /// by +1 (the system prompt).
    #[tokio::test]
    async fn fork_session_message_count_is_non_system_only() {
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "hi back".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let state = sample_state_with_provider(provider);
        let app = build_router(state);

        // 1) Create a session.
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

        // 2) Send a user message. After the forwarder drains the runtime
        //    emits a MessageAppended for the user message AND the assistant
        //    reply, so the non-system count goes from 0 → 2 (user +
        //    assistant). The system prompt is filtered out.
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/sessions/{src_id}/messages"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({
                            "content": "ping"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "send_message must succeed");

        // 3) Fork the session. The forked session should report
        //    `message_count: 2` (non-system only). Pre-fix, this returned
        //    `3` because `transcript_snapshot.len()` includes the system
        //    prompt that was appended by the runtime builder.
        let resp = app
            .clone()
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

        let count = fork["message_count"]
            .as_u64()
            .expect("message_count must be a number");
        assert_eq!(
            count, 2,
            "fork message_count must reflect non-system messages only (user +              assistant), got: {fork}"
        );

        // 4) Cross-check: the new session's `GET /sessions` entry must
        //    report the SAME message_count so a client that calls fork
        //    and then lists sessions sees a consistent number.
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let list: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let fork_id = fork["id"].as_str().unwrap();
        let listed = list["sessions"]
            .as_array()
            .expect("sessions is an array")
            .iter()
            .find(|s| s["id"].as_str() == Some(fork_id))
            .expect("forked session must appear in GET /sessions");
        assert_eq!(
            listed["message_count"], 2,
            "GET /sessions must agree with fork response, got: {listed}"
        );
    }

    /// list_sessions must return sessions sorted by id so that paginated
    /// requests are stable across calls (HashMap iteration order is random).
    #[tokio::test]
    async fn list_sessions_stable_sort_by_id() {
        let state = sample_state();
        let app = build_router(state.clone());

        // Create several sessions so the HashMap has multiple entries.
        for _ in 0..5 {
            app.clone()
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
        }

        // Fetch the full list twice and assert both pages are identical and sorted.
        let fetch = || {
            let app = build_router(state.clone());
            async move {
                let resp = app
                    .oneshot(
                        axum::http::Request::builder()
                            .method("GET")
                            .uri("/sessions")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(resp.status(), 200);
                let bytes = resp.into_body().collect().await.unwrap().to_bytes();
                // Goal-293: response is `{ total, sessions }`. Walk the
                // `sessions` array to extract ids.
                let resp: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                resp["sessions"]
                    .as_array()
                    .expect("sessions should be an array")
                    .iter()
                    .map(|s| s["id"].as_str().unwrap().to_string())
                    .collect::<Vec<_>>()
            }
        };

        let ids_a = fetch().await;
        let ids_b = fetch().await;

        assert!(!ids_a.is_empty(), "should have sessions");
        assert_eq!(ids_a, ids_b, "list_sessions must return a stable order");

        // Verify the returned ids are sorted lexicographically.
        let mut sorted = ids_a.clone();
        sorted.sort();
        assert_eq!(ids_a, sorted, "list_sessions ids must be sorted by id");
    }

    // ── Goal-293: GET /sessions envelope (`total` + paginated slice) ─────

    /// `GET /sessions?limit=2&offset=0` returns `total=3` (the un-paginated
    /// count) and a `sessions` slice of exactly 2 entries. The remaining
    /// session shows up on the next page.
    #[tokio::test]
    async fn list_sessions_envelope_total_equals_unpaginated_count() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let state = sample_state_with_provider(provider);
        let app = build_router(state.clone());

        // Create 3 sessions.
        for _ in 0..3 {
            let resp = app
                .clone()
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
            assert_eq!(resp.status(), 201);
        }

        // First page: limit=2, offset=0 → 2 items, total=3.
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/sessions?limit=2&offset=0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            page["total"], 3,
            "total must reflect the un-paginated session count"
        );
        let sessions = page["sessions"]
            .as_array()
            .expect("sessions must be an array");
        assert_eq!(
            sessions.len(),
            2,
            "page slice must respect limit (limit=2 → 2 items)"
        );

        // Second page: limit=2, offset=2 → 1 item, total still 3.
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/sessions?limit=2&offset=2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(page["total"], 3, "total stays at 3 across pages");
        let sessions = page["sessions"]
            .as_array()
            .expect("sessions must be an array");
        assert_eq!(sessions.len(), 1, "offset=2 leaves exactly one item");

        // Concatenating both pages yields all 3 distinct ids in stable order.
        let mut seen: Vec<String> = Vec::new();
        for s in page["sessions"].as_array().unwrap() {
            seen.push(s["id"].as_str().unwrap().to_string());
        }
        // Re-fetch page 1 to collect its ids too — keeps the test self-contained.
        let resp = build_router(state)
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/sessions?limit=2&offset=0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        for s in page["sessions"].as_array().unwrap() {
            seen.push(s["id"].as_str().unwrap().to_string());
        }
        let mut unique: Vec<String> = seen.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 3, "both pages together cover all 3 sessions");
    }
}

// ===========================================================================
// Issue #105 — inbound triggers + outbound notifications
// ===========================================================================

#[cfg(feature = "http")]
pub(crate) mod trigger_endpoints {
    use super::common::{mock_config, SET_INSECURE_OK};
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt as _;
    use recursive::http::{build_router, AppState, Metrics, RateLimiter};
    use recursive::llm::{Completion, MockProvider};
    use recursive::tools::ToolRegistry;
    use recursive::triggers::{Trigger, TriggerSpec, TriggerStore};
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    pub(crate) fn state_with_workspace(ws: &std::path::Path) -> AppState {
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
            tools: vec![],
            config: {
                let mut c = mock_config();
                c.workspace = ws.to_path_buf();
                c
            },
            tool_registry: ToolRegistry::local(),
            provider,
            event_channels: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(Metrics::default()),
            slash_commands: Arc::new(Vec::new()),
            host: Arc::new(recursive::session_host::SessionHost::new(
                std::time::Duration::from_secs(0),
                recursive::http::AdmissionGate::new(
                    8,
                    std::time::Duration::ZERO,
                    Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    Arc::new(std::sync::atomic::AtomicU64::new(0)),
                ),
            )),
            rate_limiter: RateLimiter::new(100, 1.0),
            skills: vec![],
            storage: Arc::new(recursive::storage::LocalStorageBackend::new(
                std::env::temp_dir().join(format!("trig-test-{}", std::process::id())),
            )),
            agui_active_runs: Arc::new(std::sync::Mutex::new(HashMap::new())),
            // Issue #121: no native session mirror for these fixtures.
            session_mirror_root: None,
        }
    }

    /// State whose workspace is a private tempdir, so trigger stores in
    /// tests never collide (TriggerStore keys on `config.workspace`).
    fn trigger_state() -> (tempfile::TempDir, AppState) {
        let ws = tempfile::tempdir().expect("workspace tempdir");
        let state = state_with_workspace(ws.path());
        (ws, state)
    }

    async fn get_json(app: axum::Router, uri: &str) -> (axum::http::StatusCode, serde_json::Value) {
        let resp = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
        };
        (status, json)
    }

    async fn post_json(
        app: axum::Router,
        uri: &str,
        body: serde_json::Value,
    ) -> (axum::http::StatusCode, serde_json::Value) {
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
        };
        (status, json)
    }

    #[tokio::test]
    async fn create_cron_trigger_round_trips_and_computes_next_fire() {
        let (ws, state) = trigger_state();
        let app = build_router(state);
        let (status, body) = post_json(
            app.clone(),
            "/triggers",
            serde_json::json!({
                "id": "trig-morning",
                "kind": "cron",
                "cron": "30 9 * * *",
                "goal": "morning summary",
                "enabled": true
            }),
        )
        .await;
        assert_eq!(status, 201, "create cron trigger: {body}");
        assert_eq!(body["kind"], "cron");
        assert_eq!(body["cron"], "30 9 * * *");
        assert_eq!(body["enabled"], true);
        // next_fire_at was computed at registration (not null).
        let next = body["next_fire_at"].as_str().expect("next_fire_at set");
        assert!(recursive::triggers::parse_rfc3339_utc(next).is_some());

        // Persisted under the workspace's user dir.
        let store = TriggerStore::for_workspace(ws.path());
        let stored = store.get("trig-morning").expect("load").expect("stored");
        assert_eq!(stored.goal, "morning summary");
        assert!(stored.enabled);
    }

    #[tokio::test]
    async fn create_webhook_trigger_echoes_secret_once() {
        let (ws, state) = trigger_state();
        let app = build_router(state);
        let (status, body) = post_json(
            app.clone(),
            "/triggers",
            serde_json::json!({"kind": "webhook", "goal": "g"}),
        )
        .await;
        assert_eq!(status, 201);
        let secret = body["secret"].as_str().expect("secret echoed on create");
        assert_eq!(secret.len(), 32);
        let id = body["id"].as_str().expect("id").to_string();
        assert_eq!(
            body["webhook_path"],
            serde_json::json!(format!("/webhooks/{id}?key={secret}")),
            "the create response must hand back a ready-to-use fire URL"
        );

        // Every *later* read goes through the real handler and must carry
        // neither the secret nor the key — asserted on the wire, not on a
        // local re-implementation of the serializer.
        let (status, list) = get_json(app.clone(), "/triggers").await;
        assert_eq!(status, 200, "list: {list}");
        let entry = list
            .as_array()
            .expect("array")
            .iter()
            .find(|t| t["id"] == id.as_str())
            .expect("created trigger listed");
        assert!(
            entry["secret"].is_null(),
            "listing must not echo the secret: {entry}"
        );
        assert_eq!(
            entry["webhook_path"],
            serde_json::json!(format!("/webhooks/{id}"))
        );
        assert!(
            !entry["webhook_path"]
                .as_str()
                .unwrap_or("")
                .contains(secret),
            "listing must not leak the key: {entry}"
        );

        let (status, one) = get_json(app.clone(), &format!("/triggers/{id}")).await;
        assert_eq!(status, 200, "get: {one}");
        assert!(one["secret"].is_null(), "get must not echo the secret");
        assert!(!one["webhook_path"].as_str().unwrap_or("").contains(secret));

        // The stored spec still keeps the secret — verifying inbound
        // calls is the whole point; only the responses omit it.
        let store = TriggerStore::for_workspace(ws.path());
        let stored = store.get(&id).expect("get").expect("stored");
        assert!(
            matches!(&stored.spec, TriggerSpec::Webhook { secret: s } if s.as_str() == secret),
            "stored trigger keeps the secret: {:?}",
            stored.spec
        );
    }

    #[tokio::test]
    async fn create_trigger_rejects_bad_cron_and_kind() {
        let (_ws, state) = trigger_state();
        let app = build_router(state);
        let (status, body) = post_json(
            app.clone(),
            "/triggers",
            serde_json::json!({"kind": "cron", "cron": "99 * * * *", "goal": "g"}),
        )
        .await;
        assert_eq!(status, 400, "minute 99 must be rejected: {body}");
        let (status, _) = post_json(
            app.clone(),
            "/triggers",
            serde_json::json!({"kind": "signal", "goal": "g"}),
        )
        .await;
        assert_eq!(status, 400, "unknown kind must be rejected");
        let (status, _) = post_json(
            app,
            "/triggers",
            serde_json::json!({"kind": "cron", "cron": "0 9 * * *", "goal": "  "}),
        )
        .await;
        assert_eq!(status, 400, "empty goal must be rejected");
    }

    /// Regression (issue #105 review): a malformed webhook notify URL must
    /// fail the create call, not surface hours later as a fire-time
    /// delivery error.
    #[tokio::test]
    async fn create_trigger_rejects_malformed_notify_webhook_url() {
        let (ws, state) = trigger_state();
        let app = build_router(state);
        for bad in ["not a url", "example.com/hook", "file:///etc/passwd"] {
            let (status, body) = post_json(
                app.clone(),
                "/triggers",
                serde_json::json!({
                    "id": "trig-bad-notify",
                    "kind": "cron",
                    "cron": "0 9 * * *",
                    "goal": "g",
                    "notify": {"kind": "webhook", "url": bad},
                }),
            )
            .await;
            assert_eq!(status, 400, "'{bad}' must be rejected: {body}");
        }
        // Nothing was persisted for the rejected registrations.
        let store = TriggerStore::for_workspace(ws.path());
        assert!(
            store.get("trig-bad-notify").expect("load").is_none(),
            "a rejected registration must not be stored"
        );

        // A well-formed target still registers.
        let (status, body) = post_json(
            app,
            "/triggers",
            serde_json::json!({
                "id": "trig-ok-notify",
                "kind": "cron",
                "cron": "0 9 * * *",
                "goal": "g",
                "notify": {"kind": "webhook", "url": "https://example.com/hook"},
            }),
        )
        .await;
        assert_eq!(status, 201, "valid notify target: {body}");
    }

    #[tokio::test]
    async fn trigger_crud_list_get_delete_patch() {
        let (_ws, state) = trigger_state();
        let app = build_router(state);
        let (status, _created) = post_json(
            app.clone(),
            "/triggers",
            serde_json::json!({"id": "trig-crud", "kind": "cron", "cron": "0 9 * * *", "goal": "g"}),
        )
        .await;
        assert_eq!(status, 201);

        // GET one.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/triggers/trig-crud")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let got: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(got["id"], "trig-crud");
        assert_eq!(got["enabled"], false, "created disabled by default");

        // LIST contains it.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/triggers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let list: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let ids: Vec<&str> = list
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["id"].as_str())
            .collect();
        assert!(ids.contains(&"trig-crud"));

        // PATCH enable.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/triggers/trig-crud")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"enabled":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let patched: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(patched["enabled"], true);

        // DELETE.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/triggers/trig-crud")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 204);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/triggers/trig-crud")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }

    // clippy::await_holding_lock: the std env guard deliberately spans the
    // fire's awaits — only same-process trigger tests contend for the
    // process-global FILE_ROOT it protects (same posture as
    // `agui_prompt_fixture` in src/http/handlers.rs).
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn webhook_fire_runs_goal_and_delivers_notify() {
        // FILE_ROOT is process-global; serialize against the scheduler
        // test so the bound root cannot be swapped mid-fire. The std
        // guard deliberately spans the awaits below — only same-process
        // trigger tests contend (same posture as `agui_prompt_fixture`).
        let _guard = recursive::test_util::env_lock();
        let (ws, state) = trigger_state();
        // Bind the notify file context to this workspace BEFORE the fire.
        recursive::notify::set_file_context(ws.path());
        let sink = recursive::notify::allowed_file_root(ws.path()).join("sink.jsonl");

        // Register the trigger directly in the store (enabled, with a
        // file notify target) — the create handler is covered above.
        let store = TriggerStore::for_workspace(ws.path());
        let mut trigger = Trigger::new(
            "trig-hook",
            TriggerSpec::Webhook {
                secret: "s3cret".into(),
            },
            "say the word",
            None,
            Some(recursive::notify::NotifyTarget::File { path: sink.clone() }),
        );
        trigger.enabled = true;
        store.upsert(trigger).expect("seed");

        let app = build_router(state);
        // Fire without a key → 401.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhooks/trig-hook")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 401, "missing key must be unauthorized");
        // Fire with the wrong key → 401.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhooks/trig-hook?key=wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
        // Fire correctly → 202.
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhooks/trig-hook?key=s3cret")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"extra":"context"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 202, "fire accepted");

        // The run is queued as a fire-and-forget task (202 above), so wait
        // for it to record its outcome. `stamp_result` runs *after* the
        // notify delivery, so once `last_result` is set the sink is final.
        let store = TriggerStore::for_workspace(ws.path());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let updated = loop {
            let t = store.get("trig-hook").expect("get").expect("present");
            if t.last_result.is_some() {
                break t;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "fire never recorded its result"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };

        let content = std::fs::read_to_string(&sink).expect("notify sink written");
        let line: serde_json::Value = serde_json::from_str(content.trim()).expect("jsonl line");
        assert_eq!(line["source"], "webhook:trig-hook");
        // One-shot trigger runs persist under a `trigger-run/` key, not a
        // registered session id.
        assert!(
            line["session_id"]
                .as_str()
                .unwrap_or("")
                .starts_with("trigger-run/"),
            "one-shot run keyed under trigger-run/: {line}"
        );

        assert!(updated.last_fired_at.is_some());
        let result = updated.last_result.unwrap_or_default();
        assert!(
            result.contains("notified") || result.contains("finished"),
            "last_result records the outcome: {result}"
        );
    }

    /// The handler takes the per-trigger fence before queuing, so a retry
    /// while a run is still in flight gets the documented 409 instead of a
    /// second concurrent run.
    #[tokio::test]
    async fn webhook_fire_returns_409_while_a_run_is_in_flight() {
        let (ws, state) = trigger_state();
        let host = Arc::clone(&state.host);
        let store = TriggerStore::for_workspace(ws.path());
        let mut trigger = Trigger::new(
            "trig-busy",
            TriggerSpec::Webhook { secret: "k".into() },
            "g",
            None,
            None,
        );
        trigger.enabled = true;
        store.upsert(trigger).expect("seed");
        // Simulate an in-flight run for this trigger.
        let _held = host
            .try_begin_run("trigger:trig-busy")
            .expect("fence free to start with");
        let app = build_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhooks/trig-busy?key=k")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 409, "in-flight run must conflict");
    }

    #[tokio::test]
    async fn webhook_fire_refuses_unknown_disabled_and_cron_ids() {
        let (ws, state) = trigger_state();
        let store = TriggerStore::for_workspace(ws.path());
        let mut cron = Trigger::new(
            "trig-cron",
            TriggerSpec::Cron {
                expr: "0 9 * * *".into(),
            },
            "g",
            None,
            None,
        );
        cron.enabled = true;
        store.upsert(cron).expect("seed cron");
        store
            .upsert(Trigger::new(
                "trig-disabled",
                TriggerSpec::Webhook {
                    secret: String::new(),
                },
                "g",
                None,
                None,
            ))
            .expect("seed disabled");
        let app = build_router(state);

        // Unknown id → 404.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhooks/trig-nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        // Cron id → 400 (fires on schedule, not via HTTP).
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhooks/trig-cron")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        // Disabled → 409.
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhooks/trig-disabled")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 409);
    }

    #[tokio::test]
    async fn openapi_documents_trigger_endpoints() {
        let ws = tempfile::tempdir().expect("workspace tempdir");
        let app = build_router(state_with_workspace(ws.path()));
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/openapi.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let spec: serde_json::Value = serde_json::from_slice(&body).unwrap();
        for path in ["/triggers", "/triggers/{id}", "/webhooks/{id}"] {
            assert!(spec["paths"].get(path).is_some(), "openapi missing {path}");
        }
        for schema in ["CreateTriggerRequest", "TriggerResponse", "NotifyTarget"] {
            assert!(
                spec["components"]["schemas"].get(schema).is_some(),
                "openapi missing schema {schema}"
            );
        }
    }

    /// The cron scheduler fires a due trigger end-to-end: a cron with
    /// next_fire_at in the past gets picked up on the first tick, the run
    /// happens against the mock provider, and the schedule advances.
    // See webhook_fire_runs_goal_and_delivers_notify for the allow rationale.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn scheduler_fires_due_cron_and_advances() {
        use recursive::http::triggers as trig_http;
        // FILE_ROOT is process-global; serialize against the webhook test.
        // The std guard deliberately spans the awaits below (see the
        // webhook test's note).
        let _guard = recursive::test_util::env_lock();
        let (ws, state) = trigger_state();
        recursive::notify::set_file_context(ws.path());
        let store = TriggerStore::for_workspace(ws.path());
        let fired = std::sync::Arc::new(AtomicBool::new(false));
        let mut trigger = Trigger::new(
            "trig-sched",
            TriggerSpec::Cron {
                expr: "0 9 * * *".into(),
            },
            "scheduled work",
            None,
            None,
        );
        trigger.enabled = true;
        trigger.next_fire_at = Some("2000-01-01T00:00:00Z".into()); // long due
        store.upsert(trigger).expect("seed");

        let _handle = trig_http::spawn_trigger_scheduler(
            std::sync::Arc::new(state),
            std::time::Duration::from_millis(50),
        );
        // Phase 1: the scheduler consumes the due slot (advance past the
        // seeded 2000 timestamp, stamping the "firing" placeholder).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let advanced = store.get("trig-sched").expect("get").expect("present");
            if advanced.next_fire_at.as_deref() != Some("2000-01-01T00:00:00Z") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "scheduler never advanced the due trigger"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        // Phase 2: the spawned fire completes the run and overwrites the
        // placeholder with the real outcome.
        loop {
            let advanced = store.get("trig-sched").expect("get").expect("present");
            let result = advanced.last_result.as_deref().unwrap_or("");
            if result.contains("finished") || result.contains("error") || result.contains("failed")
            {
                assert!(
                    result.contains("finished"),
                    "one-shot run completed against the mock provider: {result}"
                );
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "fire never recorded its result (last_result still {result:?})"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let advanced = store.get("trig-sched").expect("get").expect("present");
        assert!(advanced.last_fired_at.is_some(), "fire was stamped");
        let _ = fired;
        let _ = ws;
    }
}

#[cfg(feature = "http")]
mod session_message_notify {
    use super::trigger_endpoints::state_with_workspace;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use recursive::http::build_router;
    use recursive::notify;
    use tower::ServiceExt;

    /// POST /sessions then /sessions/:id/messages with a file notify
    /// target: the response carries `notify_result` and the sink file
    /// contains this turn's final text.
    // See trigger_endpoints::webhook_fire_runs_goal_and_delivers_notify
    // for the allow rationale (process-global FILE_ROOT + std env lock).
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn session_message_with_notify_delivers_result() {
        // Same allow posture as the other trigger tests: the std env lock
        // spans awaits; only same-process tests contend.
        let _guard = recursive::test_util::env_lock();
        let ws = tempfile::tempdir().expect("ws");
        notify::set_file_context(ws.path());
        let sink = notify::allowed_file_root(ws.path()).join("turns.jsonl");
        let _ = std::fs::remove_file(&sink);

        let app = build_router(state_with_workspace(ws.path()));
        // Create a session.
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"system_prompt":"test"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let created: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let sid = created["id"].as_str().unwrap().to_string();

        // Send a message with a notify target.
        let body = serde_json::json!({
            "content": "hello",
            "notify": {"kind": "file", "path": sink.to_string_lossy()},
        });
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/sessions/{sid}/messages"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let reply: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let result = reply["notify_result"].as_str().expect("notify_result set");
        assert!(
            result.contains("notified via file"),
            "delivery must succeed: {result}"
        );

        let content = std::fs::read_to_string(&sink).expect("sink written");
        let line: serde_json::Value = serde_json::from_str(content.trim()).expect("jsonl");
        assert_eq!(line["session_id"], sid.as_str());
        assert_eq!(line["source"], "session:message");
        assert_eq!(line["final_text"], "hello");
    }

    /// Without `notify` the response has no `notify_result` field (exact
    /// backward compatibility).
    #[tokio::test]
    async fn session_message_without_notify_omits_field() {
        let ws = tempfile::tempdir().expect("ws");
        let app = build_router(state_with_workspace(ws.path()));
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"system_prompt":"test"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let sid: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let sid = sid["id"].as_str().unwrap().to_string();

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/sessions/{sid}/messages"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"content":"hi"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let reply: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            reply.get("notify_result").is_none(),
            "no notify requested → no field: {reply}"
        );
    }
}
