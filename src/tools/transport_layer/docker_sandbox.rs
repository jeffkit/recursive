//! Docker-backed shell tool for L2 container sandbox.
//!
//! [`DockerShellTool`] implements the [`Tool`] trait with the same `run_shell`
//! name and argument schema as [`crate::tools::shell::RunShell`], but executes commands
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

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::tools::Tool;
use crate::tools::ToolSideEffect;

/// Docker-backed replacement for [`crate::tools::shell::RunShell`].
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
                        binds: Some(vec![format!("{workspace_str}:/workspace:ro")]),
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
                name: "Bash".into(),
                call_id: None,
                message: format!("docker exec create: {e}"),
            })?;

        let mut output = String::new();
        match self
            .docker
            .start_exec(&exec.id, None)
            .await
            .map_err(|e| Error::Tool {
                name: "Bash".into(),
                call_id: None,
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
            name: "Bash".into(),
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
                name: "Bash".into(),
                message: "missing required argument: command".into(),
            })?;
        self.exec_command(command).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;

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

    // ── Issue #245 batch 30 (B1 seat) — Drop / execute pins ─────────────────
    //
    // These drive the real bollard client against a loopback fake Docker API
    // (a raw HTTP/1.1 listener that records the request line and answers 204),
    // so the container-removal semantics of `Drop` are observable without a
    // Docker daemon. `Docker::connect_with_http` only builds the client
    // struct — the connection is lazy — so a literal-constructed
    // `DockerShellTool` never dials anywhere until a tool call does.

    /// Spawn a fake Docker daemon on 127.0.0.1 that records every request
    /// line and answers `204 No Content`. Returns its `http://` address and
    /// the shared record buffer.
    async fn spawn_fake_docker_api() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = format!("http://{}", listener.local_addr().expect("local addr"));
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let reqs = requests.clone();
        tokio::spawn(async move {
            loop {
                let (sock, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => return,
                };
                let reqs = reqs.clone();
                tokio::spawn(async move {
                    let mut sock = sock;
                    let mut buf = vec![0u8; 8192];
                    let mut read = 0;
                    loop {
                        let n = sock.read(&mut buf[read..]).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        read += n;
                        if buf[..read].windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let text = String::from_utf8_lossy(&buf[..read]).to_string();
                    let first = text.lines().next().unwrap_or("").to_string();
                    reqs.lock().unwrap_or_else(|e| e.into_inner()).push(first);
                    let _ = sock
                        .write_all(
                            b"HTTP/1.1 204 No Content\r\nconnection: close\r\ncontent-length: 0\r\n\r\n",
                        )
                        .await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (addr, requests)
    }

    /// `DockerShellTool`'s `Drop` must issue `DELETE /containers/{id}` (via
    /// bollard's `remove_container`) so the per-instance container cannot
    /// outlive the tool. Kills `replace <impl Drop>::drop with ()`: with the
    /// mutant no request ever reaches the fake daemon and the wait times out
    /// at the "must issue a remove" assert. Kills `delete field force from
    /// RemoveContainerOptions`: bollard serialises every bool of the options
    /// struct (no `skip_serializing_if`), so the mutant's `force=false`
    /// request line lacks `force=true` and the force assert goes red while
    /// the arrival assert stays green.
    #[tokio::test]
    async fn drop_removes_container_with_force_via_docker_api() {
        let (addr, requests) = spawn_fake_docker_api().await;
        let docker = Docker::connect_with_http(&addr, 120, bollard::API_DEFAULT_VERSION)
            .expect("lazy client must build");
        let tool = DockerShellTool {
            docker,
            container_id: "fake-container-id".to_string(),
            timeout: Duration::from_secs(5),
        };
        drop(tool);

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let snap = requests.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let remove = snap
                .iter()
                .find(|l| l.starts_with("DELETE ") && l.contains("/containers/fake-container-id"));
            if let Some(line) = remove {
                assert!(
                    line.contains("force=true"),
                    "container removal must set force=true (a lingering exited \
                     container would otherwise accumulate); got: {line}"
                );
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "drop must issue a remove_container call against the docker \
                 API; requests seen: {snap:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// `execute` without a `command` argument must be a `BadToolArgs` error,
    /// raised before any docker call (the fake address above is never
    /// dialled on this path). Kills `replace execute -> Result<String> with
    /// Ok("xyzzy".into())`: the mutant returns Ok for the same input and the
    /// is_err assert goes red.
    #[tokio::test]
    async fn execute_rejects_missing_command_argument() {
        let docker =
            Docker::connect_with_http("http://127.0.0.1:1", 5, bollard::API_DEFAULT_VERSION)
                .expect("lazy client must build");
        let tool = DockerShellTool {
            docker,
            container_id: "unused".to_string(),
            timeout: Duration::from_secs(1),
        };
        let err = tool.execute(json!({})).await;
        assert!(
            err.is_err(),
            "missing `command` must be rejected, got: {:?}",
            err
        );
        let msg = err.expect_err("missing command must error").to_string();
        assert!(
            msg.contains("missing required argument: command"),
            "error must name the missing argument; got: {msg}"
        );
    }
}
