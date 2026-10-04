//! Goal 397 — HTTP session cold load: restart recovery from storage.
//!
//! The "restart" is simulated at the seam the issue prescribes: transcripts
//! are written into a `LocalStorageBackend` (what Goal 396's write path
//! flushes), the server process is a fresh `AppState` whose in-memory table
//! starts empty, and the HTTP API must restore the session on first access.
//!
//! Run with: `cargo test --test http_cold_load`
#![cfg(feature = "http")]

// Shared fixtures (mock_config / SET_INSECURE_OK / sample_state_with_storage).
#[path = "http_common/mod.rs"]
mod common;

mod cold_load_tests {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use recursive::http::build_router;
    use recursive::llm::{Completion, MockProvider, ToolCall};
    use recursive::message::{Message, Role};
    use recursive::storage::{LocalStorageBackend, StorageBackend};
    use std::sync::Arc;
    use tower::ServiceExt;

    use crate::common::{sample_state_with_storage, SET_INSECURE_OK};

    fn msg(role: Role, content: &str) -> Message {
        Message {
            role,
            content: content.to_string(),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: None,
            is_compaction_summary: false,
        }
    }

    fn assistant_with_tool_call(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: id.to_string(),
                name: "Read".to_string(),
                arguments: serde_json::json!({"path": "a.txt"}),
            }],
            tool_call_id: None,
            reasoning_content: None,
            is_compaction_summary: false,
        }
    }

    fn tool_result(call_id: &str) -> Message {
        Message {
            role: Role::Tool,
            content: "file body".to_string(),
            tool_calls: vec![],
            tool_call_id: Some(call_id.to_string()),
            reasoning_content: None,
            is_compaction_summary: false,
        }
    }

    fn stop_completion(content: &str) -> Completion {
        Completion {
            content: content.into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }
    }

    /// A storage backend rooted at a fresh temp dir — stands in for the
    /// transcripts a previous server process flushed before exiting.
    fn fresh_storage() -> (tempfile::TempDir, Arc<LocalStorageBackend>) {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(LocalStorageBackend::new(dir.path().to_path_buf()));
        (dir, backend)
    }

    async fn seed(backend: &LocalStorageBackend, id: &str, msgs: Vec<Message>) {
        backend.save_transcript(id, &msgs).await.unwrap();
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> (axum::http::StatusCode, serde_json::Value) {
        let builder = axum::http::Request::builder().method(method).uri(uri);
        let request = match body {
            Some(v) => builder
                .header("content-type", "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, json)
    }

    fn non_system_roles(detail: &serde_json::Value) -> Vec<String> {
        detail["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] != "system")
            .map(|m| m["role"].as_str().unwrap().to_string())
            .collect()
    }

    // ── tests ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn get_restores_session_after_restart() {
        let (_dir, backend) = fresh_storage();
        seed(
            &backend,
            "cold-get-1",
            vec![
                msg(Role::System, "stored prompt"),
                msg(Role::User, "ping"),
                msg(Role::Assistant, "pong"),
            ],
        )
        .await;

        SET_INSECURE_OK.call_once(|| {
            unsafe { std::env::set_var("RECURSIVE_HTTP_AUTH_INSECURE_OK", "1") };
        });
        let app = build_router(sample_state_with_storage(
            Arc::new(MockProvider::new(vec![])),
            backend,
        ));

        let (status, detail) = send(&app, "GET", "/sessions/cold-get-1", None).await;
        assert_eq!(status, 200);
        assert_eq!(detail["id"], "cold-get-1");
        // Restored content is visible, with exactly ONE system message — the
        // runtime's own prompt; the stored one was stripped, not seeded.
        let messages = detail["messages"].as_array().unwrap();
        assert_eq!(messages.iter().filter(|m| m["role"] == "system").count(), 1);
        assert_eq!(non_system_roles(&detail), vec!["user", "assistant"]);
        assert_eq!(messages[1]["content"], "ping");
        assert_eq!(messages[2]["content"], "pong");
    }

    #[tokio::test]
    async fn post_continues_restored_conversation_with_legal_pairing() {
        let (_dir, backend) = fresh_storage();
        // Seeded transcript includes a complete tool round — the restored
        // session must keep it legal (invariant #8) when the turn continues.
        seed(
            &backend,
            "cold-post-1",
            vec![
                msg(Role::User, "first"),
                assistant_with_tool_call("call-1"),
                tool_result("call-1"),
                msg(Role::Assistant, "first reply"),
            ],
        )
        .await;

        SET_INSECURE_OK.call_once(|| {
            unsafe { std::env::set_var("RECURSIVE_HTTP_AUTH_INSECURE_OK", "1") };
        });
        let app = build_router(sample_state_with_storage(
            Arc::new(MockProvider::new(vec![stop_completion("second reply")])),
            backend,
        ));

        let (status, reply) = send(
            &app,
            "POST",
            "/sessions/cold-post-1/messages",
            Some(serde_json::json!({"content": "again"})),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(reply["content"], "second reply");

        // The transcript kept growing on top of the restored turns.
        let (_, detail) = send(&app, "GET", "/sessions/cold-post-1", None).await;
        assert_eq!(
            non_system_roles(&detail),
            vec![
                "user",
                "assistant",
                "tool",
                "assistant",
                "user",
                "assistant"
            ]
        );
        // Invariant #8 walk over the rendered transcript: every tool message
        // is answered by the immediately preceding assistant's tool_calls.
        let messages = detail["messages"].as_array().unwrap();
        for (i, m) in messages.iter().enumerate() {
            if m["role"] == "tool" {
                let prev = &messages[i - 1];
                let call_ids: Vec<&str> = prev["tool_calls"]
                    .as_array()
                    .map(|c| c.iter().filter_map(|t| t["id"].as_str()).collect())
                    .unwrap_or_default();
                assert!(call_ids.contains(&m["tool_call_id"].as_str().unwrap()));
            }
        }
    }

    #[tokio::test]
    async fn unknown_id_stays_404_and_creates_no_ghost() {
        let (_dir, backend) = fresh_storage();
        SET_INSECURE_OK.call_once(|| {
            unsafe { std::env::set_var("RECURSIVE_HTTP_AUTH_INSECURE_OK", "1") };
        });
        let app = build_router(sample_state_with_storage(
            Arc::new(MockProvider::new(vec![])),
            backend,
        ));

        let (status, body) = send(&app, "GET", "/sessions/never-existed", None).await;
        assert_eq!(status, 404);
        assert_eq!(body["error"], "session not found");

        // No ghost session was materialized by the failed lookup.
        let (_, list) = send(&app, "GET", "/sessions", None).await;
        assert_eq!(list["total"], 0);
    }

    #[tokio::test]
    async fn orphan_tool_prefix_is_stripped_not_400() {
        let (_dir, backend) = fresh_storage();
        // A transcript whose head is orphan tool results (e.g. a crashed
        // partial flush) must restore without tripping invariant #8.
        seed(
            &backend,
            "cold-orphan-1",
            vec![
                tool_result("call-1"),
                tool_result("call-2"),
                msg(Role::User, "after orphans"),
                msg(Role::Assistant, "recovered"),
            ],
        )
        .await;

        SET_INSECURE_OK.call_once(|| {
            unsafe { std::env::set_var("RECURSIVE_HTTP_AUTH_INSECURE_OK", "1") };
        });
        let app = build_router(sample_state_with_storage(
            Arc::new(MockProvider::new(vec![])),
            backend,
        ));

        let (status, detail) = send(&app, "GET", "/sessions/cold-orphan-1", None).await;
        assert_eq!(status, 200);
        let roles = non_system_roles(&detail);
        assert_eq!(roles, vec!["user", "assistant"]);
        assert!(!roles.iter().any(|r| r == "tool"));
    }

    #[tokio::test]
    async fn system_and_orphans_only_stays_404() {
        let (_dir, backend) = fresh_storage();
        // Empty-after-normalization: nothing restorable → 404, no ghost.
        seed(
            &backend,
            "cold-empty-1",
            vec![msg(Role::System, "only a prompt"), tool_result("call-1")],
        )
        .await;

        SET_INSECURE_OK.call_once(|| {
            unsafe { std::env::set_var("RECURSIVE_HTTP_AUTH_INSECURE_OK", "1") };
        });
        let app = build_router(sample_state_with_storage(
            Arc::new(MockProvider::new(vec![])),
            backend,
        ));

        let (status, _) = send(&app, "GET", "/sessions/cold-empty-1", None).await;
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn list_sessions_stays_memory_only() {
        let (_dir, backend) = fresh_storage();
        seed(
            &backend,
            "cold-list-1",
            vec![msg(Role::User, "ping"), msg(Role::Assistant, "pong")],
        )
        .await;

        SET_INSECURE_OK.call_once(|| {
            unsafe { std::env::set_var("RECURSIVE_HTTP_AUTH_INSECURE_OK", "1") };
        });
        let app = build_router(sample_state_with_storage(
            Arc::new(MockProvider::new(vec![])),
            backend,
        ));

        // Cold load is lazy: listing never touches storage, so a persisted
        // session does not appear until it is actually accessed.
        let (_, list) = send(&app, "GET", "/sessions", None).await;
        assert_eq!(list["total"], 0);

        let (_, _) = send(&app, "GET", "/sessions/cold-list-1", None).await;
        let (_, list) = send(&app, "GET", "/sessions", None).await;
        assert_eq!(list["total"], 1);
        assert_eq!(list["sessions"][0]["id"], "cold-list-1");
    }

    /// Goal 396/397 集成语义：DELETE 保留快照（396：会话结束即落盘）**但**写
    /// tombstone，冷加载不得复活已删除会话——否则 DELETE → GET 会 200，违反 v050
    /// 生命周期契约（DELETE 之后必须 404）。驱逐/停机不写 tombstone，仍可恢复。
    #[tokio::test]
    async fn delete_marks_tombstone_so_cold_load_cannot_resurrect() {
        let (_dir, backend) = fresh_storage();
        seed(
            &backend,
            "purge-1",
            vec![msg(Role::User, "hi"), msg(Role::Assistant, "yo")],
        )
        .await;
        SET_INSECURE_OK.call_once(|| {
            unsafe { std::env::set_var("RECURSIVE_HTTP_AUTH_INSECURE_OK", "1") };
        });
        let app = build_router(sample_state_with_storage(
            Arc::new(MockProvider::new(vec![])),
            backend.clone(),
        ));

        // 首次 GET 冷加载进内存表。
        let (status, _) = send(&app, "GET", "/sessions/purge-1", None).await;
        assert_eq!(status, 200);

        let (status, _) = send(&app, "DELETE", "/sessions/purge-1", None).await;
        assert_eq!(status, 204);

        // 396 语义：会话结束（含 DELETE）仍要落盘快照——CLI 的 sessions 目录语义。
        let stored = backend.load_transcript("purge-1").await.unwrap();
        assert!(
            !stored.is_empty(),
            "delete must still persist the transcript snapshot (Goal 396)"
        );

        let (status, _) = send(&app, "GET", "/sessions/purge-1", None).await;
        assert_eq!(
            status, 404,
            "a deleted session must not be resurrected by cold load"
        );
    }

    /// Issue #98 acceptance: a session created with a custom `system_prompt`
    /// and a `permission_mode` must restore BOTH after a restart. The cold-load
    /// path used to rebuild from the server defaults — silently swapping the
    /// persona and downgrading the permission mode.
    #[tokio::test]
    async fn restart_preserves_custom_prompt_and_permission_mode() {
        let (_dir, backend) = fresh_storage();
        SET_INSECURE_OK.call_once(|| {
            unsafe { std::env::set_var("RECURSIVE_HTTP_AUTH_INSECURE_OK", "1") };
        });
        let state = sample_state_with_storage(
            Arc::new(MockProvider::new(vec![stop_completion("hello there")])),
            backend.clone(),
        );
        let app = build_router(state.clone());

        let (status, created) = send(
            &app,
            "POST",
            "/sessions",
            Some(serde_json::json!({
                "system_prompt": "You are a pirate.",
                "permission_mode": "auto",
            })),
        )
        .await;
        assert_eq!(status, 201);
        let id = created["id"].as_str().expect("session id").to_string();

        // One real turn so the transcript has restorable content (an empty —
        // system-only — transcript intentionally cold-loads to 404).
        let (status, _) = send(
            &app,
            "POST",
            &format!("/sessions/{id}/messages"),
            Some(serde_json::json!({"content": "hi"})),
        )
        .await;
        assert_eq!(status, 200);

        // Graceful shutdown (Goal 396) → the next process sees only storage.
        recursive::http::flush_all_sessions(&state).await;

        let restarted =
            sample_state_with_storage(Arc::new(MockProvider::new(vec![])), backend.clone());
        let app2 = build_router(restarted);

        let (status, detail) = send(&app2, "GET", &format!("/sessions/{id}"), None).await;
        assert_eq!(status, 200);
        let system_msg = detail["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "system")
            .expect("restored transcript carries a system message");
        assert!(
            system_msg["content"]
                .as_str()
                .unwrap()
                .contains("You are a pirate."),
            "custom system prompt must survive the restart, got: {}",
            system_msg["content"]
        );
        assert_eq!(
            detail["permission_mode"], "auto",
            "permission mode must survive the restart"
        );
        // The user prompt is still extracted from the restored transcript.
        assert_eq!(detail["first_prompt"], "hi");
        assert_eq!(detail["last_prompt"], "hi");
    }
}
