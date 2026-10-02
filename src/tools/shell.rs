//! `run_shell`: execute a command in the workspace.
//!
//! Uses `/bin/sh -c` so the model can write idiomatic one-liners (pipes,
//! redirects, etc.). Stdout and stderr are captured and returned together.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

use super::resolve_within;
use super::Tool;
use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::tools::tool_kind::ToolKind;

/// Hard ceiling for the LLM-supplied `max_output_bytes` arg. Generous
/// enough for a full `cargo build` diagnostic dump, small enough that a
/// runaway command can't exhaust the agent's memory.
const MAX_OUTPUT_BYTES_HARD_CAP: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct RunShell {
    pub root: PathBuf,
    pub timeout: Duration,
    pub max_output_bytes: usize,
    /// Goal 403: I/O backend for command execution. Defaults to
    /// [`super::transport::LocalTransport`] (byte-identical to the
    /// pre-transport behaviour); a container transport routes commands
    /// into the sandbox environment instead of the host shell.
    pub transport: std::sync::Arc<dyn super::transport::ToolTransport>,
}

impl RunShell {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            timeout: Duration::from_secs(300),
            max_output_bytes: 128 * 1024,
            transport: std::sync::Arc::new(super::transport::LocalTransport),
        }
    }

    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    /// Bind the shell to a transport (Goal 403 container tier). The
    /// transport receives host-resolved paths (`root` + optional `cwd`);
    /// container transports map them into the environment.
    pub fn with_transport(
        mut self,
        transport: std::sync::Arc<dyn super::transport::ToolTransport>,
    ) -> Self {
        self.transport = transport;
        self
    }

    /// Override the default per-stream output cap. Useful when the host
    /// wants every `Bash` call to retain more (or less) output than the
    /// 128 KiB default. The LLM can also request a larger cap per-call
    /// via the `max_output_bytes` arg; this setter only changes the
    /// baseline the per-call arg is clamped against.
    pub fn with_max_output_bytes(mut self, n: usize) -> Self {
        self.max_output_bytes = n.min(MAX_OUTPUT_BYTES_HARD_CAP);
        self
    }
}

#[async_trait]
impl Tool for RunShell {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Bash".into(),
            description:
                "Run a shell command (sh -c) from the workspace root, or from an optional subdirectory inside it via `cwd`."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Command line to execute via sh -c"
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Optional subdirectory (relative to workspace root) to run the command in. Must stay inside the workspace."
                    },
                    "env": {
                        "type": "object",
                        "description": "Optional extra env vars set for this command only. Values must be strings; non-string values are rejected. Local tiers (RECURSIVE_SANDBOX unset / none / policy) add these to (or override) the inherited host env. Sandboxed tiers (container / microvm) have no inherited env — only these explicitly passed variables exist inside the sandbox; host env (credentials included) is never forwarded.",
                        "additionalProperties": {
                            "type": "string"
                        }
                    },
                    "max_output_bytes": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Per-stream (stdout / stderr) byte cap. Defaults to the tool's configured limit (128 KiB). Pass a larger value (≤ 2 MiB) when you need the full output of a verbose command — e.g. `cargo build` error logs — and the default would truncate the relevant lines. The cap is applied per-stream; both stdout and stderr are bounded independently."
                    }
                },
                "required": ["command"]
            }),
        }
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Execute
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let command = args["command"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "Bash".into(),
            message: "missing `command`".into(),
        })?;

        // Determine the working directory: resolve optional cwd or use root.
        let cwd = if let Some(rel) = args.get("cwd").and_then(|v| v.as_str()) {
            resolve_within(&self.root, rel).map_err(|e| Error::BadToolArgs {
                name: "Bash".into(),
                message: format!("cwd: {e}"),
            })?
        } else {
            self.root.clone()
        };

        // Goal 403: delegate execution to the transport. With the default
        // `LocalTransport` this is byte-identical to the previous direct
        // `tokio::process` path (the local `exec_shell` implementation
        // preserves the kill-on-timeout / kill-on-drop semantics); a
        // container transport routes the command into the sandbox.
        let mut env_pairs: Vec<(String, String)> = Vec::new();
        if let Some(env_map) = args.get("env").and_then(|v| v.as_object()) {
            for (key, val) in env_map {
                let val_str = val.as_str().ok_or_else(|| Error::BadToolArgs {
                    name: "Bash".to_string(),
                    message: format!("env value for `{key}` must be a string, got {:?}", val),
                })?;
                env_pairs.push((key.clone(), val_str.to_string()));
            }
        }

        // LLM may ask for a larger per-stream cap when it knows it needs
        // the full output (e.g. a long `cargo build` diagnostic). Clamp
        // to the hard cap so a malformed request can't exhaust memory;
        // values below the configured default are honoured too, since a
        // smaller cap is always safe.
        let max = match args.get("max_output_bytes").and_then(|v| v.as_u64()) {
            Some(n) => (n as usize).min(MAX_OUTPUT_BYTES_HARD_CAP),
            None => self.max_output_bytes,
        };

        let result = self
            .transport
            .exec_shell(command, &cwd, &env_pairs, self.timeout, max)
            .await
            .map_err(|e| Error::Tool {
                name: "Bash".into(),
                call_id: None,
                message: super::fs::transport_io_error(&cwd, &e),
            })?;

        // Goal 400/403: the transport's structural classification is the
        // single source of truth for WHY the exec ended. Environment
        // failures (container OOM/killed) must not be reported as a plain
        // tool error the model would try to "fix" in its command; surface
        // the classification explicitly instead (contract pinned by
        // `mock_transport_surfaces_failure_classification`).
        if let Some(failure) = result.failure {
            return Err(Error::Tool {
                name: "Bash".into(),
                call_id: None,
                message: match failure {
                    super::transport::TransportFailure::Environment => format!(
                        "environment failure (sandbox container died or was killed; \
                         not a command bug): cwd={}",
                        cwd.display()
                    ),
                    super::transport::TransportFailure::Retryable => format!(
                        "retryable: transient sandbox transport failure: cwd={}",
                        cwd.display()
                    ),
                    super::transport::TransportFailure::Tool => format!(
                        "sandbox transport rejected the invocation: cwd={}",
                        cwd.display()
                    ),
                },
            });
        }

        let code = result
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into());

        Ok(format!(
            "exit: {code}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            result.stdout, result.stderr
        ))
    }
}

#[cfg(test)]
#[cfg(not(target_os = "windows"))]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn runs_echo_in_workspace() {
        let tmp = TempDir::new().unwrap();
        let out = RunShell::new(tmp.path())
            .execute(json!({"command": "echo hello && pwd"}))
            .await
            .unwrap();
        assert!(out.contains("exit: 0"));
        assert!(out.contains("hello"));
    }

    #[tokio::test]
    async fn captures_nonzero_status() {
        let tmp = TempDir::new().unwrap();
        let out = RunShell::new(tmp.path())
            .execute(json!({"command": "exit 7"}))
            .await
            .unwrap();
        assert!(out.contains("exit: 7"));
    }

    #[tokio::test]
    async fn enforces_timeout() {
        let tmp = TempDir::new().unwrap();
        let tool = RunShell::new(tmp.path()).with_timeout(Duration::from_millis(150));
        let err = tool
            .execute(json!({"command": "sleep 5"}))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Tool { .. }));
    }

    // The default 128 KiB cap truncates `cargo build`-sized diagnostics
    // before the relevant error lines. The LLM can opt out per-call by
    // passing `max_output_bytes`. We verify that:
    //   1. The arg actually raises the cap (200 KiB output is preserved).
    //   2. The hard cap (2 MiB) is enforced — a 16 MiB request still
    //      truncates well below that, so a malformed request can't OOM
    //      the agent.
    #[tokio::test]
    async fn per_call_max_output_bytes_can_raise_within_hard_cap() {
        let tmp = TempDir::new().unwrap();
        let tool = RunShell::new(tmp.path());
        // 200 KiB of `x` lines; default cap would truncate, requested
        // cap (200 * 1024) keeps it whole.
        let wanted_bytes = 200 * 1024;
        let out = tool
            .execute(json!({
                "command": format!("yes x | head -c {}", wanted_bytes),
                "max_output_bytes": wanted_bytes,
            }))
            .await
            .unwrap();
        assert!(
            !out.contains("[output truncated]"),
            "output was truncated despite the per-call cap: {out}"
        );
    }

    #[tokio::test]
    async fn per_call_max_output_bytes_clamped_to_hard_cap() {
        let tmp = TempDir::new().unwrap();
        let tool = RunShell::new(tmp.path());
        // 4 MiB request: clamped to 2 MiB hard cap. 3 MiB of output
        // therefore truncates.
        let out = tool
            .execute(json!({
                "command": "yes x | head -c 3145728",
                "max_output_bytes": 4_194_304u32,
            }))
            .await
            .unwrap();
        assert!(
            out.contains("[output truncated]"),
            "hard cap was not enforced: {out}"
        );
    }

    // Regression: before P0-A, the timeout branch returned Err without
    // killing the spawned child. `kill_on_drop(true)` and an explicit
    // `start_kill` together guarantee the child is reaped. We verify by
    // having the child `exec sleep` (so the shell PID *becomes* the
    // sleep PID — killing the child kills the actual sleeper), writing
    // that PID to a marker file, then `kill -0`-polling after the
    // timeout. Pre-fix this test hangs for the full 30s sleep; post-fix
    // the PID is gone within a couple of seconds.
    #[tokio::test]
    async fn timeout_kills_child_process() {
        let tmp = TempDir::new().unwrap();
        let marker = tmp.path().join("child.pid");
        let marker_str = marker.to_string_lossy().into_owned();
        // `exec` replaces the sh process with sleep, so the PID we
        // capture is the PID `start_kill` targets.
        let command = format!("echo $$ > {marker_str} && exec sleep 30");
        let tool = RunShell::new(tmp.path()).with_timeout(Duration::from_millis(150));
        let err = tool
            .execute(json!({ "command": command }))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Tool { .. }));

        // The child may legitimately still be writing the marker when the
        // 150 ms timeout fires (host under load: fork+exec+echo alone can
        // exceed it), so poll briefly for the file instead of reading it
        // once — a missing file at t=0 is a scheduling race, not an orphan.
        let mut pid_str = None;
        for _ in 0..50 {
            if let Ok(s) = std::fs::read_to_string(&marker) {
                pid_str = Some(s);
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let pid: i32 = pid_str
            .expect("child should have written its PID before exec")
            .trim()
            .parse()
            .expect("PID file should contain a number");

        // Poll `kill -0` for up to 5 seconds; the child must be gone.
        // We shell out instead of binding libc to honour AGENTS.md's
        // "no new deps without justification" rule.
        let mut dead = false;
        for _ in 0..50 {
            let probe = std::process::Command::new("kill")
                .arg("-0")
                .arg(pid.to_string())
                .output()
                .expect("kill -0 probe should spawn");
            if !probe.status.success() {
                dead = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            dead,
            "timed-out child PID {pid} still alive after 5s — orphan"
        );
    }

    // The timeout arm must kill the child and then AWAIT both reader
    // tasks before returning, so their JoinHandles don't detach with
    // buffered output. The observable contract: `execute` returns `Err`
    // promptly — it must NOT hang until the child's sleep would have
    // finished. `exec` makes the shell PID *become* the sleeper, so the
    // kill closes the pipes and the drain completes immediately (a plain
    // `sleep 30` would leave an orphaned descendant holding the pipes
    // and the drain would block for the full sleep — a known limitation
    // of best-effort cleanup on this path). If the drain were ordered
    // before the kill (deadlock) or the kill were dropped (readers never
    // see EOF), this test would hang for the full 30s.
    #[tokio::test]
    async fn shell_timeout_drains_reader_tasks() {
        let tmp = TempDir::new().unwrap();
        let tool = RunShell::new(tmp.path()).with_timeout(Duration::from_millis(200));
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            tool.execute(json!({"command": "exec sleep 30"})),
        )
        .await;
        let err = match result {
            Ok(r) => r.expect_err("expected the timed-out command to fail"),
            Err(_) => panic!("execute did not return within 5s — reader tasks not drained"),
        };
        match err {
            Error::Tool { message, .. } => assert!(
                message.contains("timed out"),
                "expected a timeout error, got: {message}"
            ),
            other => panic!("expected Tool timeout error, got: {other}"),
        }
    }

    #[tokio::test]
    async fn runs_in_subdir_when_cwd_given() {
        let tmp = TempDir::new().unwrap();
        // Create a subdirectory with a marker file
        let sub = tmp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("marker.txt"), "content").unwrap();

        let out = RunShell::new(tmp.path())
            .execute(json!({"command": "ls", "cwd": "sub"}))
            .await
            .unwrap();

        assert!(out.contains("exit: 0"));
        assert!(out.contains("marker.txt"));
    }

    #[tokio::test]
    async fn rejects_cwd_outside_workspace() {
        let tmp = TempDir::new().unwrap();
        let err = RunShell::new(tmp.path())
            .execute(json!({"command": "echo hello", "cwd": "../escape"}))
            .await
            .unwrap_err();

        assert!(matches!(err, Error::BadToolArgs { ref name, .. } if name == "Bash"));
        let err_msg = format!("{err}");
        assert!(err_msg.contains("cwd"));
    }

    #[tokio::test]
    async fn accepts_dot_cwd_as_root() {
        let tmp = TempDir::new().unwrap();
        let out = RunShell::new(tmp.path())
            .execute(json!({"command": "pwd", "cwd": "."}))
            .await
            .unwrap();

        assert!(out.contains("exit: 0"));
        // pwd should output something non-empty
        assert!(out.contains("--- stdout ---"));
    }

    #[tokio::test]
    async fn existing_no_cwd_call_still_works() {
        let tmp = TempDir::new().unwrap();
        let out = RunShell::new(tmp.path())
            .execute(json!({"command": "echo hello"}))
            .await
            .unwrap();

        assert!(out.contains("exit: 0"));
        assert!(out.contains("hello"));
    }

    // ── Builder method tests ──────────────────────────────────────────────────

    #[test]
    fn with_timeout_stores_value() {
        // kills `replace RunShell::with_timeout -> RunShell with Default::default()`
        let tmp = TempDir::new().unwrap();
        let d = Duration::from_millis(777);
        let tool = RunShell::new(tmp.path()).with_timeout(d);
        assert_eq!(tool.timeout, d, "with_timeout must persist the duration");
    }

    #[test]
    fn with_max_output_bytes_clamps_to_hard_cap() {
        // kills `n.min(MAX_OUTPUT_BYTES_HARD_CAP)` → `n` mutant
        let tmp = TempDir::new().unwrap();
        let tool = RunShell::new(tmp.path()).with_max_output_bytes(MAX_OUTPUT_BYTES_HARD_CAP * 4);
        assert_eq!(
            tool.max_output_bytes, MAX_OUTPUT_BYTES_HARD_CAP,
            "with_max_output_bytes must clamp at the hard cap"
        );
    }

    #[test]
    fn with_max_output_bytes_within_cap_is_preserved() {
        // complementary: values within the hard cap are stored as-is
        let tmp = TempDir::new().unwrap();
        let tool = RunShell::new(tmp.path()).with_max_output_bytes(64 * 1024);
        assert_eq!(
            tool.max_output_bytes,
            64 * 1024,
            "with_max_output_bytes must store values within the hard cap unchanged"
        );
    }

    // Tests for env-vars passthrough (goal-27)
    #[tokio::test]
    async fn env_overrides_and_errors() {
        let tmp = TempDir::new().unwrap();
        let tool = RunShell::new(tmp.path());

        // Test A: env var is set and visible in the command
        let out = tool
            .execute(json!({"command": "echo $RECURSIVE_TEST_VAR", "env": {"RECURSIVE_TEST_VAR": "hello"}}))
            .await
            .unwrap();
        assert!(out.contains("exit: 0"));
        assert!(out.contains("hello"));

        // Test B: non-string env value returns BadToolArgs
        let err = tool
            .execute(json!({"command": "echo x", "env": {"MY_KEY": 42}}))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::BadToolArgs { .. }));
        let err_msg = format!("{err}");
        assert!(
            err_msg.contains("MY_KEY"),
            "error should mention the offending key: {err_msg}"
        );

        // Test C (regression): omitting env works exactly as before
        let out = tool
            .execute(json!({"command": "echo hello"}))
            .await
            .unwrap();
        assert!(out.contains("exit: 0"));
        assert!(out.contains("hello"));
    }

    // Issue #49②: the `env` arg description sent to the model must match
    // the actual per-tier behaviour. It must NOT claim a blanket
    // "inherited env" (false in the container/microvm tiers, where
    // `env_pairs` starts empty — see `.dev/AGENTS.md` invariant #3 and
    // tests/issue51_sandbox_env_inheritance.rs); it must state both the
    // local-tier inheritance and the sandbox-tier non-inheritance.
    #[test]
    fn env_schema_description_matches_per_tier_reality() {
        let tmp = TempDir::new().unwrap();
        let params = RunShell::new(tmp.path()).spec().parameters;
        let desc = params["properties"]["env"]["description"]
            .as_str()
            .expect("env property must carry a description");

        assert!(
            !desc.contains("the inherited env"),
            "env description must not blanket-claim an inherited env \
             (false for container/microvm tiers): {desc}"
        );
        assert!(
            desc.contains("inherited host env"),
            "env description must state that LOCAL tiers inherit the host env: {desc}"
        );
        assert!(
            desc.contains("no inherited env") || desc.contains("never forwarded"),
            "env description must state that SANDBOX tiers do not inherit \
             host env: {desc}"
        );
    }
}
