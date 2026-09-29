//! Issue #31 (Goal 404) — session-scoped environment binding, lib-level
//! tests (plan §C: migrated from `tests/wip_environment_binding.rs`,
//! `wip_` prefix dropped, names carry `environment_binding` so
//! `cargo test --lib environment_binding` collects them all).
//!
//! Coverage:
//!  1. PINS the hard requirement that `sandbox = none` keeps the system
//!     prompt byte-identical (baseline snapshot).
//!  2. The `<environment>` segment appears only for a non-local transport
//!     and never leaks host paths / credentials.
//!  3. `assemble_system_prompt_with_environment(None)` is byte-identical to
//!     the legacy `assemble_system_prompt` (wrapper contract).
//!  4. `ToolTransport::destroy()` contract: default no-op for the local
//!     transport; a counting fake pins that the trait hook is reachable.
//!  5. `AgentRuntime::destroy_environment` drains the session's background
//!     job manager and destroys the transport exactly once, idempotently.

use crate::assemble_system_prompt;
use crate::assemble_system_prompt_with_environment;
use crate::tools::transport::{EnvironmentCapabilities, ExecResult, LocalTransport, ToolTransport};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn environment_binding_sandbox_none_prompt_is_pinned_byte_for_byte() {
    let tmp = tempfile::tempdir().unwrap();
    // No AGENTS.md / CLAUDE.md in the temp workspace: rules segment empty.
    let a = assemble_system_prompt("BASE", tmp.path(), &[], false).into_full();
    let b = assemble_system_prompt("BASE", tmp.path(), &[], false).into_full();
    assert_eq!(a, b);
    assert_eq!(
        a, "BASE",
        "no env/skills/subagents => prompt is exactly base"
    );
}

#[test]
fn environment_binding_wrapper_without_environment_is_byte_identical_to_legacy() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("AGENTS.md"), "AG").unwrap();
    let legacy = assemble_system_prompt("BASE", tmp.path(), &[], true).into_full();
    let wrapped =
        assemble_system_prompt_with_environment("BASE", tmp.path(), &[], true, None).into_full();
    assert_eq!(legacy, wrapped);
}

#[test]
fn environment_binding_segment_appears_when_provided() {
    let tmp = tempfile::tempdir().unwrap();
    let caps = EnvironmentCapabilities {
        network: false,
        persistent: true,
        path_root: PathBuf::from("/workspace"),
        user: Some("1000:1000".into()),
        toolchain: vec!["sh".into(), "git".into()],
        snapshot: false,
    };
    let seg = caps.render_environment_segment();
    assert!(seg.contains("## Environment"), "{seg}");
    assert!(seg.contains("/workspace"));
    assert!(seg.contains("network access: no"));
    let out = assemble_system_prompt_with_environment("BASE", tmp.path(), &[], false, Some(&seg))
        .into_full();
    assert!(out.starts_with("BASE"));
    assert!(out.contains("## Environment"));
    // Container-only view: no credential-ish keys in the segment.
    assert!(!out.to_lowercase().contains("api_key"));
}

#[test]
fn environment_binding_segment_not_injected_for_local_tier() {
    // The local tier must keep the prompt byte-identical: empty path_root is
    // the signal `inject_environment_segment` (http/handlers) keys on.
    let caps = EnvironmentCapabilities::local();
    assert!(
        caps.path_root.as_os_str().is_empty(),
        "local capabilities must carry an empty path_root (no segment signal)"
    );
}

/// Counting fake transport: pins that the `destroy` trait hook is callable
/// through the `dyn ToolTransport` interface, and counts calls for the
/// drain test below.
#[derive(Debug, Default)]
pub(crate) struct CountingDestroyTransport {
    pub destroy_calls: AtomicU64,
}

#[async_trait::async_trait]
impl ToolTransport for CountingDestroyTransport {
    async fn read_file(&self, _path: &std::path::Path) -> std::io::Result<Vec<u8>> {
        Ok(Vec::new())
    }
    async fn write_file(&self, _path: &std::path::Path, _contents: &[u8]) -> std::io::Result<()> {
        Ok(())
    }
    async fn list_dir(
        &self,
        _path: &std::path::Path,
    ) -> std::io::Result<Vec<crate::tools::transport::DirEntry>> {
        Ok(Vec::new())
    }
    async fn create_dir_all(&self, _path: &std::path::Path) -> std::io::Result<()> {
        Ok(())
    }
    async fn exec_shell(
        &self,
        _command: &str,
        _cwd: &std::path::Path,
        _env: &[(String, String)],
        _timeout: Duration,
        _max_output_bytes: usize,
    ) -> std::io::Result<ExecResult> {
        Ok(ExecResult::default())
    }
    async fn destroy(&self) {
        self.destroy_calls.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn environment_binding_destroy_reachable_through_dyn_transport() {
    let t: std::sync::Arc<dyn ToolTransport> =
        std::sync::Arc::new(CountingDestroyTransport::default());
    t.destroy().await;
    let c = t.capabilities(); // unrelated read; force dyn dispatch usability
    assert_eq!(c, EnvironmentCapabilities::local());
}

#[tokio::test]
async fn environment_binding_local_transport_destroy_is_noop() {
    LocalTransport.destroy().await;
    LocalTransport.destroy().await;
}

/// Plan §D: session destroy drains the background-job manager and destroys
/// the transport exactly once; repeated destroy_environment is a no-op.
#[tokio::test]
async fn environment_binding_destroy_drains_background_jobs() {
    use crate::tools::run_background::{BackgroundJobManager, Job, JobState};
    use crate::AgentRuntime;
    use std::time::Instant;

    let transport = Arc::new(CountingDestroyTransport::default());
    // Seed the manager FIRST, then build the registry through the real
    // builder with Some(manager): the registry must end up holding the SAME
    // manager the background tools use, so destroy_environment drains real
    // jobs. (Issue #31 review: rewiring the field by hand in the test hid
    // the builder never wiring it back.)
    let manager = Arc::new(tokio::sync::Mutex::new(BackgroundJobManager::new()));
    manager.lock().await.insert(Job {
        state: JobState::Running,
        created_at: Instant::now(),
    });
    assert!(
        Arc::ptr_eq(
            crate::tools::registry::build_standard_tools_with_transport(
                transport.clone(),
                &std::env::temp_dir(),
                &[],
                None,
                &[],
                30,
                None,
                None,
                None,
                Some(manager.clone()),
            )
            .bg_manager(),
            &manager,
        ),
        "builder must wire the registry's manager to the one the tools hold"
    );

    let registry = crate::tools::registry::build_standard_tools_with_transport(
        transport.clone(),
        &std::env::temp_dir(),
        &[],
        None,
        &[],
        30,
        None,
        None,
        None,
        Some(manager.clone()),
    );

    let mut runtime = AgentRuntime::builder()
        .llm(Arc::new(crate::llm::MockProvider::new(vec![])))
        .tools(registry)
        .system_prompt("sys".to_string())
        .build()
        .unwrap();

    runtime.destroy_environment().await;

    assert!(
        manager.lock().await.get_state("bg-1").is_none(),
        "session destroy must drain the background-job manager"
    );
    assert_eq!(
        transport.destroy_calls.load(Ordering::SeqCst),
        1,
        "transport destroyed exactly once"
    );

    // Idempotent: a repeated destroy neither re-destroys nor errors.
    runtime.destroy_environment().await;
    assert_eq!(
        transport.destroy_calls.load(Ordering::SeqCst),
        1,
        "repeated destroy_environment must not re-destroy the transport"
    );
}
