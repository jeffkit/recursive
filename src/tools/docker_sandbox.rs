//! Docker-backed shell tool for L2 container sandbox.
//!
//! [`DockerShellTool`] implements the [`Tool`] trait with the same `run_shell`
//! name and argument schema as [`super::RunShell`], but executes commands
//! inside a Docker container instead of the host process. The workspace
//! directory is bind-mounted into the container so file IO is shared.
//!
//! Gated behind the `cloud-runtime` feature flag.

use std::time::Duration;

use async_trait::async_trait;
use bollard::container::{Config, RemoveContainerOptions};
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::models::HostConfig;
use bollard::Docker;
use futures_util::StreamExt;
use serde_json::{json, Value};

use super::Tool;
use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::tools::ToolSideEffect;

/// Docker-backed replacement for [`super::RunShell`].
///
/// Each instance owns one container that is cleaned up on `Drop`.
/// Commands run as `sh -c <command>` inside the container at `/workspace`.
pub struct DockerShellTool {
    docker: Docker,
    container_id: String,
    /// Default timeout per exec call.
    timeout: Duration,
}

impl DockerShellTool {
    /// Start a new container from `image` and bind-mount `workspace`.
    ///
    /// Resource limits:
    /// - Memory: 512 MB
    /// - CPU: 1 core (nano_cpus = 1_000_000_000)
    pub async fn new(image: &str, workspace: &std::path::Path, timeout_secs: u64) -> Result<Self> {
        let docker = Docker::connect_with_local_defaults().map_err(|e| Error::Storage {
            message: format!("docker connect: {e}"),
        })?;

        let workspace_str = workspace.to_str().unwrap_or(".");
        let container = docker
            .create_container::<String, String>(
                None,
                Config {
                    image: Some(image.to_string()),
                    working_dir: Some("/workspace".to_string()),
                    // Keep stdin open so the container doesn't exit immediately.
                    open_stdin: Some(true),
                    tty: Some(true),
                    host_config: Some(HostConfig {
                        binds: Some(vec![format!("{workspace_str}:/workspace")]),
                        memory: Some(512 * 1024 * 1024),
                        nano_cpus: Some(1_000_000_000),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| Error::Storage {
                message: format!("docker create container: {e}"),
            })?;

        docker
            .start_container::<String>(&container.id, None)
            .await
            .map_err(|e| Error::Storage {
                message: format!("docker start container {}: {e}", container.id),
            })?;

        Ok(Self {
            docker,
            container_id: container.id,
            timeout: Duration::from_secs(timeout_secs),
        })
    }

    /// Execute `command` (via `sh -c`) inside the container.
    pub async fn exec_command(&self, command: &str) -> Result<String> {
        let exec = self
            .docker
            .create_exec(
                &self.container_id,
                CreateExecOptions {
                    cmd: Some(vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        command.to_string(),
                    ]),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| Error::Tool {
                name: "run_shell".into(),
                message: format!("docker exec create: {e}"),
            })?;

        let mut output = String::new();
        match self
            .docker
            .start_exec(&exec.id, None)
            .await
            .map_err(|e| Error::Tool {
                name: "run_shell".into(),
                message: format!("docker exec start: {e}"),
            })? {
            StartExecResults::Attached {
                output: mut stream, ..
            } => {
                let deadline = tokio::time::sleep(self.timeout);
                tokio::pin!(deadline);
                loop {
                    tokio::select! {
                        _ = &mut deadline => break,
                        chunk = stream.next() => match chunk {
                            Some(Ok(msg)) => output.push_str(&msg.to_string()),
                            _ => break,
                        },
                    }
                }
            }
            StartExecResults::Detached => {}
        }
        Ok(output)
    }
}

impl Drop for DockerShellTool {
    fn drop(&mut self) {
        let docker = self.docker.clone();
        let id = self.container_id.clone();
        tokio::spawn(async move {
            let _ = docker
                .remove_container(
                    &id,
                    Some(RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await;
        });
    }
}

#[async_trait]
impl Tool for DockerShellTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_shell".into(),
            description: "Run a shell command inside an isolated Docker container at /workspace."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Command line to execute via sh -c inside the container"
                    }
                },
                "required": ["command"]
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::External
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let command = arguments
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::BadToolArgs {
                name: "run_shell".into(),
                message: "missing required argument: command".into(),
            })?;
        self.exec_command(command).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn docker_shell_executes_echo() {
        if std::env::var("RECURSIVE_TEST_DOCKER").is_err() {
            return; // skip when Docker is not available
        }
        let dir = TempDir::new().unwrap();
        let tool = DockerShellTool::new("ubuntu:22.04", dir.path(), 30)
            .await
            .expect("start container");
        let out = tool.exec_command("echo hello").await.expect("exec");
        assert!(out.contains("hello"), "expected 'hello' in: {out}");
    }
}
