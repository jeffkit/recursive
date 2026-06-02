# Goal 187 — CloudRuntime: DockerToolProvider（L2 容器沙盒）

**Roadmap**: Phase 21.3 — CloudRuntime（3/4）

**依赖**: Goal 183（ToolSetProvider trait）合并后开始；与 Goal 185/186 可并行

**Feature flag**: `cloud-runtime`

**Design principle check**:
- 新增 `src/tools/docker_provider.rs`，实现 `ToolSetProvider` trait
- 工具实现中的 `run_shell` 替换为 Docker exec；文件 IO 走挂载 volume
- ❌ 不修改 `agent.rs` 主循环

## Why

L1 策略沙盒（Goal 184）只做规则过滤，无法阻止内核漏洞利用。
L2 容器沙盒通过 Docker/gVisor 实现进程级 namespace + cgroup 隔离：
- `run_shell` → Docker exec API（命令在容器内执行，不触达宿主机）
- 文件 IO → 工作目录挂载为容器 volume，容器内读写，宿主机同步

适合云端商业 SaaS 部署场景。

## Scope

### 1. `Cargo.toml` — 更新 `cloud-runtime` feature，增加 Docker 依赖

```toml
cloud-runtime = [
    ...,
    "dep:bollard",
]

[dependencies]
bollard = { version = "0.18", optional = true }
```

### 2. 新建 `src/tools/docker_sandbox.rs`

实现 `DockerShellTool`——将 `run_shell` 重定向到 Docker exec：

```rust
//! Docker-backed shell tool for L2 container sandbox.

use bollard::Docker;
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::container::{CreateContainerOptions, StartContainerOptions, Config};
use futures_util::StreamExt;
use crate::error::{Error, Result};

/// Executes shell commands inside a Docker container.
///
/// The container is created fresh per session and removed on Drop.
/// The workspace directory is bind-mounted into the container at /workspace.
pub struct DockerShellTool {
    docker: Docker,
    container_id: String,
    workspace: std::path::PathBuf,
}

impl DockerShellTool {
    /// Spin up a new container for this session.
    pub async fn new(
        image: &str,
        workspace: std::path::PathBuf,
        timeout_secs: u64,
    ) -> Result<Self> {
        let docker = Docker::connect_with_local_defaults().map_err(|e| Error::Storage {
            message: format!("docker connect: {e}"),
        })?;

        let workspace_str = workspace.to_str().unwrap_or(".");
        let container = docker
            .create_container::<&str, &str>(
                None,
                Config {
                    image: Some(image),
                    working_dir: Some("/workspace"),
                    host_config: Some(bollard::models::HostConfig {
                        binds: Some(vec![format!("{workspace_str}:/workspace")]),
                        // Restrict resources for multi-tenant safety
                        memory: Some(512 * 1024 * 1024), // 512 MB
                        nano_cpus: Some(1_000_000_000),   // 1 CPU
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| Error::Storage { message: format!("docker create: {e}") })?;

        docker.start_container(&container.id, None::<StartContainerOptions<&str>>)
            .await
            .map_err(|e| Error::Storage { message: format!("docker start: {e}") })?;

        Ok(Self {
            docker,
            container_id: container.id,
            workspace,
        })
    }

    /// Execute a command inside the container.
    pub async fn exec(&self, command: &str, timeout_secs: u64) -> Result<String> {
        let exec = self.docker.create_exec(
            &self.container_id,
            CreateExecOptions {
                cmd: Some(vec!["sh", "-c", command]),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                ..Default::default()
            },
        ).await.map_err(|e| Error::Storage { message: format!("docker exec create: {e}") })?;

        let mut output = String::new();
        if let StartExecResults::Attached { mut output: stream, .. } =
            self.docker.start_exec(&exec.id, None).await
                .map_err(|e| Error::Storage { message: format!("docker exec start: {e}") })?
        {
            let deadline = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs));
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    _ = &mut deadline => break,
                    chunk = stream.next() => match chunk {
                        Some(Ok(msg)) => output.push_str(&msg.to_string()),
                        _ => break,
                    }
                }
            }
        }
        Ok(output)
    }
}

impl Drop for DockerShellTool {
    fn drop(&mut self) {
        let docker = self.docker.clone();
        let id = self.container_id.clone();
        tokio::spawn(async move {
            let _ = docker.remove_container(&id, Some(bollard::container::RemoveContainerOptions {
                force: true,
                ..Default::default()
            })).await;
        });
    }
}
```

### 3. 新建 `src/tools/docker_provider.rs`，实现 `ToolSetProvider`

```rust
//! Docker-based ToolSetProvider for L2 container sandbox.

use crate::tool_set_provider::{SandboxMode, ToolSetProvider};
use crate::tools::ToolRegistry;

pub struct DockerToolSetProvider {
    pub image: String,
    pub workspace: std::path::PathBuf,
}

impl ToolSetProvider for DockerToolSetProvider {
    fn build_registry(&self) -> ToolRegistry {
        let mut registry = crate::tools::build_default_registry();
        // Replace run_shell with DockerShellTool at the aliases level.
        // The DockerShellTool is registered as "run_shell" primary name,
        // so LLM tool calls transparently route to the container.
        // (Implementation detail: override the "run_shell" entry in registry)
        registry.register_docker_shell(self.image.clone(), self.workspace.clone());
        registry
    }
    fn sandbox_mode(&self) -> SandboxMode {
        SandboxMode::Container
    }
}
```

### 4. 测试策略

- 单元测试仅在 `RECURSIVE_TEST_DOCKER` 环境变量存在时运行（需要 Docker daemon）
- 测试：容器启动 → 执行 `echo hello` → 输出包含 "hello" → 容器被清理

## Acceptance

- `cargo build --features cloud-runtime` 编译通过
- `cargo test --workspace` 绿色（无 Docker 时 skip）
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `DockerToolSetProvider` 实现 `ToolSetProvider`
- 无 `cloud-runtime` feature 时构建不受影响

## Notes for the agent

- `bollard` 是 Docker daemon API 的纯 async Rust 客户端，主流选择。
- 容器镜像推荐默认值：`ubuntu:22.04`，可通过配置覆盖。
- workspace 挂载是 bind mount，容器内改的文件宿主机可见，宿主机改的容器内可见——
  这保证了工具调用产出的文件能被后续工具读取。
- 每个 session 启动一个容器，session 结束时销毁（Drop 清理）。
- 资源限制（512MB RAM, 1 CPU）是默认值，可通过 `DockerToolSetProvider` 配置项覆盖。
