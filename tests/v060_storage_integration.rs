//! Integration tests for Goal 181–189: compute-storage separation traits.
//!
//! Covers the full stack: StorageBackend / SessionStore / ToolSetProvider
//! as injected into `AgentKernelBuilder`. No real external services required
//! for the base feature set; cloud-runtime gated tests use serialisation
//! unit checks only.

mod v060 {
    use recursive::llm::{mock::MockProvider, Completion};
    use recursive::storage::{AgentCheckpointState, LocalStorageBackend, NoopSessionStore};
    use recursive::tools::policy_sandbox::{PolicyConfig, ShellPolicy};
    use recursive::{
        AgentKernel, LocalStorageBackend as LibLocalStorage, NoopSessionStore as LibNoop,
        PolicyConfig as LibPolicyConfig, PolicyToolSetProvider, ToolSetProvider,
    };
    use std::sync::Arc;
    use tempfile::TempDir;

    // ─── helpers ─────────────────────────────────────────────────────────────

    fn mock_llm() -> Arc<dyn recursive::ChatProvider> {
        Arc::new(MockProvider::new(vec![Completion {
            content: "done".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]))
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 1. LocalStorageBackend — transcript round-trip via AgentKernelBuilder
    // ─────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn local_storage_backend_round_trip() {
        use recursive::storage::StorageBackend;

        let dir = TempDir::new().unwrap();
        let backend = Arc::new(LocalStorageBackend::new(dir.path().to_path_buf()));

        // Write two messages through the backend directly.
        use recursive::message::{Message, Role};
        let msgs = vec![
            Message {
                role: Role::User,
                content: "ping".into(),
                tool_calls: vec![],
                tool_call_id: None,
                reasoning_content: None,
                is_compaction_summary: false,
            },
            Message {
                role: Role::Assistant,
                content: "pong".into(),
                tool_calls: vec![],
                tool_call_id: None,
                reasoning_content: None,
                is_compaction_summary: false,
            },
        ];

        backend
            .save_transcript("test-session", &msgs)
            .await
            .unwrap();

        // Load back and verify equality.
        let loaded = backend.load_transcript("test-session").await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].content, "ping");
        assert_eq!(loaded[1].content, "pong");
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 2. LocalStorageBackend — memory round-trip
    // ─────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn local_storage_memory_round_trip() {
        use recursive::storage::StorageBackend;

        let dir = TempDir::new().unwrap();
        let backend = LocalStorageBackend::new(dir.path().to_path_buf());

        backend
            .save_memory("user.md", "## preferences\n- concise")
            .await
            .unwrap();

        let val = backend.load_memory("user.md").await.unwrap();
        assert_eq!(val.as_deref(), Some("## preferences\n- concise"));

        let missing = backend.load_memory("nonexistent.md").await.unwrap();
        assert!(missing.is_none());
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 3. NoopSessionStore — save/load never errors
    // ─────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn noop_session_store_save_load_does_not_error() {
        use recursive::storage::SessionStore;

        let store = NoopSessionStore;
        let state = AgentCheckpointState {
            step: 3,
            transcript_len: 12,
        };

        // Save should be a no-op and succeed.
        store.save_state("sess-abc", &state).await.unwrap();

        // Load always returns None.
        let loaded = store.load_state("sess-abc").await.unwrap();
        assert!(loaded.is_none());
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 4. AgentKernelBuilder — defaults compile and build without panicking
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn kernel_builder_defaults_build_successfully() {
        let kernel = AgentKernel::builder()
            .llm(mock_llm())
            .build()
            .expect("builder with defaults should succeed");

        // Verify default storage is a LocalStorageBackend (non-null arc).
        // We can't easily downcast dyn trait, so just assert it's accessible.
        let _ = kernel.storage();
        let _ = kernel.session_store();
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 5. AgentKernelBuilder — explicit LocalStorageBackend injection
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn kernel_builder_accepts_explicit_storage_backend() {
        let dir = TempDir::new().unwrap();
        let backend = Arc::new(LibLocalStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(LibNoop);

        let _kernel = AgentKernel::builder()
            .llm(mock_llm())
            .with_storage(backend)
            .with_session_store(store)
            .build()
            .expect("explicit storage + session store should build");
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 6. PolicyToolSetProvider — blocks forbidden shell commands
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn policy_sandbox_blocks_forbidden_command() {
        let dir = TempDir::new().unwrap();
        let policy = LibPolicyConfig {
            shell: ShellPolicy {
                deny_patterns: vec!["rm".into()],
            },
            ..Default::default()
        };
        let provider = PolicyToolSetProvider::new(dir.path().to_path_buf(), 30, vec![], policy);
        let registry = provider.build_registry();

        let attached = registry.policy().expect("policy should be attached");
        assert!(
            attached.check_shell("rm -rf /").is_err(),
            "rm should be blocked"
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 7. PolicyToolSetProvider — allows non-forbidden commands
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn policy_sandbox_allows_permitted_command() {
        let dir = TempDir::new().unwrap();
        let policy = PolicyConfig {
            shell: ShellPolicy {
                deny_patterns: vec!["rm".into()],
            },
            ..Default::default()
        };
        let provider = PolicyToolSetProvider::new(dir.path().to_path_buf(), 30, vec![], policy);
        let registry = provider.build_registry();

        let attached = registry.policy().expect("policy should be attached");
        assert!(
            attached.check_shell("ls -la").is_ok(),
            "ls should be allowed"
        );
        assert!(
            attached.check_shell("cargo test").is_ok(),
            "cargo test should be allowed"
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 8. PolicyToolSetProvider restrictive preset blocks dangerous patterns
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn restrictive_policy_preset_blocks_dangerous_commands() {
        let dir = TempDir::new().unwrap();
        let provider = PolicyToolSetProvider::restrictive(dir.path().to_path_buf(), 30, vec![]);
        let registry = provider.build_registry();

        let policy = registry.policy().unwrap();
        // Patterns from PolicyConfig::default_restrictive
        assert!(policy.check_shell("rm -rf /").is_err());
        assert!(policy.check_shell("mkfs.ext4 /dev/sda").is_err());
        assert!(policy.check_shell("echo foo > /dev/mem").is_err());
        // Safe commands should pass
        assert!(policy.check_shell("ls -la").is_ok());
        assert!(policy.check_shell("cargo build").is_ok());
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 9. AgentKernelBuilder — with_tool_set_provider injects PolicyProvider
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn kernel_builder_with_policy_provider_builds() {
        let dir = TempDir::new().unwrap();
        let provider = Arc::new(PolicyToolSetProvider::restrictive(
            dir.path().to_path_buf(),
            30,
            vec![],
        ));

        let _kernel = AgentKernel::builder()
            .llm(mock_llm())
            .with_tool_set_provider(provider)
            .build()
            .expect("kernel with policy provider should build");
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 10. AgentCheckpointState serialization (no external service needed)
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn checkpoint_state_serde_round_trip() {
        let state = AgentCheckpointState {
            step: 7,
            transcript_len: 42,
        };
        let json = serde_json::to_string(&state).expect("serialise");
        let loaded: AgentCheckpointState = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(loaded.step, 7);
        assert_eq!(loaded.transcript_len, 42);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 11. [cloud-runtime] RedisSessionStore — construction succeeds (lazy connect)
    // ─────────────────────────────────────────────────────────────────────────

    #[cfg(feature = "cloud-runtime")]
    #[test]
    fn redis_session_store_construction_succeeds() {
        use recursive::storage::SessionStore;
        use recursive::RedisSessionStore;
        use std::time::Duration;

        // Deadpool-redis creates the pool lazily: the constructor should succeed
        // even if no Redis is running.
        let store =
            RedisSessionStore::new("redis://127.0.0.1:6379", Duration::from_secs(300), "test:")
                .expect("should build from URL");

        // We can verify that the store implements SessionStore without I/O.
        let _: &dyn SessionStore = &store;
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 12. [cloud-runtime] AgentCheckpointState — JSON round-trip for Redis path
    // ─────────────────────────────────────────────────────────────────────────

    #[cfg(feature = "cloud-runtime")]
    #[test]
    fn checkpoint_state_json_is_stable() {
        use recursive::storage::AgentCheckpointState;

        let state = AgentCheckpointState {
            step: 99,
            transcript_len: 256,
        };
        let json = serde_json::to_string(&state).unwrap();
        // Verify field names are as expected (Redis stores raw JSON).
        assert!(json.contains("\"step\""));
        assert!(json.contains("\"transcript_len\""));

        let back: AgentCheckpointState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.step, 99);
        assert_eq!(back.transcript_len, 256);
    }
}
