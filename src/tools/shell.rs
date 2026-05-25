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
            name: "run_shell".into(),
            description:
                "Run a shell command (sh -c) from the workspace root. Returns combined stdout/stderr and exit status."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Command line to execute via sh -c"}
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let command = args["command"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "run_shell".into(),
            message: "missing `command`".into(),
        })?;

        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(command);
        cmd.current_dir(&self.root);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| Error::Tool {
            name: "run_shell".into(),
            message: format!("spawn failed: {e}"),
        })?;

        let mut stdout = child.stdout.take().expect("stdout piped");
        let mut stderr = child.stderr.take().expect("stderr piped");

        let max = self.max_output_bytes;
        let stdout_task = tokio::spawn(async move { read_capped(&mut stdout, max).await });
        let stderr_task = tokio::spawn(async move { read_capped(&mut stderr, max).await });

        let wait = child.wait();
        let status = match tokio::time::timeout(self.timeout, wait).await {
            Ok(s) => s.map_err(|e| Error::Tool {
                name: "run_shell".into(),
                message: format!("wait failed: {e}"),
            })?,
            Err(_) => {
                return Err(Error::Tool {
                    name: "run_shell".into(),
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
}
