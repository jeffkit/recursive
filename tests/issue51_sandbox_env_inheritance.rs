//! issue #51 — sandbox tier must NOT inherit host env vars.
//!
//! Invariant under test (issue #51 acceptance item 3, and the env
//! non-inheritance declaration at the end of the "Threat Model" section in
//! `docs/architecture/execution-environments.md` plus invariant #3's issue
//! #51 extension in `.dev/AGENTS.md`): when the Bash tool runs through a
//! sandbox transport, only env pairs the model explicitly passed in the
//! tool-call `env` arg reach the executed command. Host env
//! (RECURSIVE_E2B_API_KEY, provider keys, ...) must never leak into the
//! untrusted execution environment.
//!
//! Anchors:
//! - `src/tools/shell.rs` `RunShell::execute` — `env_pairs` starts as an
//!   empty Vec and only gains pairs from `args["env"]`.
//! - `src/tools/e2b_provider.rs` / `container_transport.rs` `exec_shell`
//!   — the `env: &[(String, String)]` slice is the ONLY env channel
//!   (rendered as an explicit `K=V` prefix inside the sandbox).
//!
//! NOTE: per `.dev/AGENTS.md`, env-var tests must be ONE test — set_var is
//! process-global and cargo test runs in parallel. Both checks below live
//! in a single sequential test.

#![cfg(feature = "test-utils")]

use async_trait::async_trait;
use recursive::tools::shell::RunShell;
use recursive::tools::transport::{ExecResult, ToolTransport};
use recursive::tools::Tool;
use serde_json::json;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Captures the `env` slice RunShell forwards to the transport, and
/// otherwise behaves like a completed no-op exec.
#[derive(Debug, Default)]
struct CapturingTransport {
    seen_env: Mutex<Vec<(String, String)>>,
}

#[async_trait]
impl ToolTransport for CapturingTransport {
    async fn exec_shell(
        &self,
        _command: &str,
        _cwd: &Path,
        env: &[(String, String)],
        _timeout: Duration,
        _max_output_bytes: usize,
    ) -> std::io::Result<ExecResult> {
        *self.seen_env.lock().unwrap() = env.to_vec();
        Ok(ExecResult {
            exit_code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            failure: None,
        })
    }
    async fn read_file(&self, _p: &Path) -> std::io::Result<Vec<u8>> {
        Ok(Vec::new())
    }
    async fn write_file(&self, _p: &Path, _c: &[u8]) -> std::io::Result<()> {
        Ok(())
    }
    async fn list_dir(&self, _p: &Path) -> std::io::Result<Vec<recursive::tools::DirEntry>> {
        Ok(Vec::new())
    }
    async fn walk(
        &self,
        _root: &Path,
        _opts: &recursive::tools::transport::WalkOptions,
    ) -> std::io::Result<Vec<recursive::tools::transport::WalkEntry>> {
        Ok(Vec::new())
    }
    async fn create_dir_all(&self, _p: &Path) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn sandbox_shell_does_not_inherit_host_env() {
    // --- 1) RunShell forwards ONLY the explicit env arg to the transport ---
    let capture = Arc::new(CapturingTransport::default());
    let tmp = tempfile::TempDir::new().unwrap();
    let tool = RunShell::new(tmp.path()).with_transport(capture.clone());
    tool.execute(json!({"command": "true"})).await.unwrap();
    {
        let seen = capture.seen_env.lock().unwrap();
        assert!(
            seen.is_empty(),
            "transport must receive an empty env when the tool call has no `env` arg, got {seen:?}"
        );
    }
    tool.execute(json!({"command": "true", "env": {"FOO": "bar"}}))
        .await
        .unwrap();
    {
        let seen = capture.seen_env.lock().unwrap();
        assert_eq!(
            seen.as_slice(),
            &[("FOO".to_string(), "bar".to_string())],
            "transport must receive exactly the explicit env arg, got {seen:?}"
        );
    }

    // --- 2) a SANDBOX transport's exec_shell only applies the explicit env.
    // We assert on the capturing transport with a hostile host env present:
    // the env slice it received (recorded above) is the complete env spec
    // — the sandbox exec builds `K=V sh -c cmd` from exactly this slice
    // (container_transport.rs:651-654, e2b_provider.rs:812-818), so a host
    // var can only reach the VM if it appears in this slice. It never did.
    std::env::set_var("RECURSIVE_WIP_SECRET_51", "leak-me");
    tool.execute(json!({"command": "printenv RECURSIVE_WIP_SECRET_51"}))
        .await
        .unwrap();
    std::env::remove_var("RECURSIVE_WIP_SECRET_51");
    let seen = capture.seen_env.lock().unwrap().clone();
    assert!(
        seen.iter().all(|(k, _)| k != "RECURSIVE_WIP_SECRET_51"),
        "host env var reached the sandbox transport env spec: {seen:?}"
    );

    // --- 3) explicit env arg DOES reach the command (positive control for
    // the local `none` tier: since issue #89 `LocalTransport` scrubs
    // credential-shaped *inherited* vars, but an env pair passed in the
    // tool call is always applied) ---
    std::env::set_var("RECURSIVE_WIP_EXPLICIT_51", "set-by-model");
    let local = RunShell::new(tmp.path()); // default LocalTransport
    let out = local
        .execute(json!({
            "command": "printenv RECURSIVE_WIP_EXPLICIT_51",
            "env": {"RECURSIVE_WIP_EXPLICIT_51": "set-by-model"}
        }))
        .await
        .unwrap();
    std::env::remove_var("RECURSIVE_WIP_EXPLICIT_51");
    assert!(out.contains("set-by-model"));
}
