//! `run_shell`: execute a command in the workspace.
//!
//! Uses `/bin/sh -c` so the model can write idiomatic one-liners (pipes,
//! redirects, etc.). Stdout and stderr are captured and returned together.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use super::resolve_within;
use super::Tool;
use crate::error::{Error, Result};
use crate::llm::ToolSpec;

#[derive(Debug, Clone)]
pub struct RunShell {
    pub root: PathBuf,
    pub timeout: Duration,
    pub max_output_bytes: usize,
}

impl RunShell {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            timeout: Duration::from_secs(300),
            max_output_bytes: 128 * 1024,
        }
    }

    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
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
                        "description": "Optional extra env vars set for this command only. Values must be strings; non-string values are rejected. These add to (or override) the inherited env.",
                        "additionalProperties": {
                            "type": "string"
                        }
                    }
                },
                "required": ["command"]
            }),
        }
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

        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(command);
        cmd.current_dir(&cwd);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        // Defence in depth against orphan processes on timeout: Tokio's
        // `Child` defaults to `kill_on_drop = false`, so a bare `return Err`
        // in the timeout branch would leave the shell and any of its
        // descendants running. The timeout branch also calls `start_kill`
        // explicitly so the intent is visible at the call site, but
        // `kill_on_drop(true)` covers the case where a future refactor
        // adds another early return (panic, `?` propagation, etc.).
        cmd.kill_on_drop(true);

        // Apply optional env overrides
        if let Some(env_map) = args.get("env").and_then(|v| v.as_object()) {
            for (key, val) in env_map {
                let val_str = val.as_str().ok_or_else(|| Error::BadToolArgs {
                    name: "Bash".to_string(),
                    message: format!("env value for `{key}` must be a string, got {:?}", val),
                })?;
                cmd.env(key, val_str);
            }
        }

        let mut child = cmd.spawn().map_err(|e| Error::Tool {
            name: "Bash".into(),
            call_id: None,
            message: format!("spawn failed: {e}"),
        })?;

        let mut stdout = child.stdout.take().ok_or_else(|| Error::Tool {
            name: "Bash".into(),
            call_id: None,
            message: "stdout was not piped".into(),
        })?;
        let mut stderr = child.stderr.take().ok_or_else(|| Error::Tool {
            name: "Bash".into(),
            call_id: None,
            message: "stderr was not piped".into(),
        })?;

        let max = self.max_output_bytes;
        let stdout_task = tokio::spawn(async move { read_capped(&mut stdout, max).await });
        let stderr_task = tokio::spawn(async move { read_capped(&mut stderr, max).await });

        let wait = child.wait();
        let status = match tokio::time::timeout(self.timeout, wait).await {
            Ok(s) => s.map_err(|e| Error::Tool {
                name: "Bash".into(),
                call_id: None,
                message: format!("wait failed: {e}"),
            })?,
            Err(_) => {
                // Best-effort SIGKILL of the timed-out process group.
                // `kill_on_drop(true)` set at spawn is the safety net,
                // but explicit kill here ensures the OS reaps the child
                // promptly rather than waiting for the Drop to fire when
                // the error path returns. `start_kill` is non-blocking
                // and tolerant of the child having already exited.
                let _ = child.start_kill();
                return Err(Error::Tool {
                    name: "Bash".into(),
                    call_id: None,
                    message: format!("command timed out after {:?}", self.timeout),
                });
            }
        };

        let out = stdout_task.await.unwrap_or_default();
        let err = stderr_task.await.unwrap_or_default();
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into());

        Ok(format!(
            "exit: {code}\n--- stdout ---\n{out}\n--- stderr ---\n{err}"
        ))
    }
}

async fn read_capped<R: AsyncReadExt + Unpin>(reader: &mut R, max: usize) -> String {
    let mut buf = Vec::with_capacity(8 * 1024);
    let mut tmp = [0u8; 8 * 1024];
    loop {
        match reader.read(&mut tmp).await {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() + n > max {
                    let take = max.saturating_sub(buf.len());
                    buf.extend_from_slice(&tmp[..take]);
                    buf.extend_from_slice(b"\n... [output truncated]");
                    let _ = tokio::io::copy(reader, &mut tokio::io::sink()).await;
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
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

        let pid_str = std::fs::read_to_string(&marker)
            .expect("child should have written its PID before exec");
        let pid: i32 = pid_str
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
}
