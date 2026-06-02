# Goal 189 — Phase 4: E2B MicroVM ToolProvider（L3 硬件级沙盒）

**Roadmap**: Phase 22 — E2B/Firecracker microVM 沙盒（L3）

**依赖**: Goal 183（ToolSetProvider trait）合并后开始；Goal 188 可先完成

**Feature flag**: `e2b-sandbox`（新增）

**Design principle check**:
- 新增 `src/tools/e2b_provider.rs`，实现 `ToolSetProvider` trait
- feature-gated，不影响默认构建
- ❌ 不修改 `agent.rs` 主循环

## Why

E2B（e2b.dev）基于 Firecracker microVM 提供：
- **硬件级隔离**：每个沙盒有独立 kernel，guest 无法逃逸到宿主机
- **<150ms 冷启动**：比传统 VM 快 10x，适合高并发场景
- **文件系统快照**（Sandbox templates）：自定义 base image，秒级启动预配置环境
- **开源核心**：`e2b-dev/E2B`，Apache 2.0

适用于：Recursive-as-a-Service（高安全托管云）、执行外部 LLM 生成代码、
多租户公有云平台。

## Scope

### 1. `Cargo.toml` — 新增 `e2b-sandbox` feature

```toml
[features]
e2b-sandbox = ["dep:reqwest"]

[dependencies]
# reqwest 已在 Cargo.toml 中，确认 feature 即可
```

E2B 提供 REST API，直接用 `reqwest` 调用即可，无需专用 SDK（或等官方 Rust SDK）。

### 2. 新建 `src/tools/e2b_provider.rs`

```rust
//! E2B Firecracker microVM-backed ToolSetProvider (L3 sandbox).
//!
//! Each session creates an isolated E2B sandbox via the REST API.
//! Commands execute inside the microVM; files are synced via the
//! filesystem API.

use crate::error::{Error, Result};
use crate::tool_set_provider::{SandboxMode, ToolSetProvider};
use crate::tools::ToolRegistry;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Configuration for an E2B sandbox.
#[derive(Clone)]
pub struct E2bConfig {
    /// E2B API key (RECURSIVE_E2B_API_KEY env var)
    pub api_key: String,
    /// Sandbox template ID (default: "base")
    pub template_id: String,
    /// Sandbox lifetime in seconds (default: 3600)
    pub timeout_secs: u32,
    /// E2B API base URL
    pub api_base: String,
}

impl E2bConfig {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            api_key: std::env::var("RECURSIVE_E2B_API_KEY").map_err(|_| Error::Config {
                message: "RECURSIVE_E2B_API_KEY not set".into(),
            })?,
            template_id: std::env::var("RECURSIVE_E2B_TEMPLATE")
                .unwrap_or_else(|_| "base".into()),
            timeout_secs: std::env::var("RECURSIVE_E2B_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(3600),
            api_base: std::env::var("RECURSIVE_E2B_API_BASE")
                .unwrap_or_else(|_| "https://api.e2b.dev".into()),
        })
    }
}

/// E2B sandbox session — wraps a live microVM.
pub struct E2bSandbox {
    config: E2bConfig,
    sandbox_id: String,
    client: reqwest::Client,
}

impl E2bSandbox {
    /// Create a new sandbox (POST /sandboxes)
    pub async fn create(config: E2bConfig) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| Error::Storage { message: format!("http client: {e}") })?;

        #[derive(Serialize)]
        struct CreateReq {
            template_id: String,
            timeout: u32,
        }
        #[derive(Deserialize)]
        struct CreateResp {
            sandbox_id: String,
        }

        let resp: CreateResp = client
            .post(format!("{}/sandboxes", config.api_base))
            .header("X-API-Key", &config.api_key)
            .json(&CreateReq {
                template_id: config.template_id.clone(),
                timeout: config.timeout_secs,
            })
            .send()
            .await
            .map_err(|e| Error::Storage { message: format!("e2b create: {e}") })?
            .json()
            .await
            .map_err(|e| Error::Storage { message: format!("e2b create parse: {e}") })?;

        Ok(Self {
            config,
            sandbox_id: resp.sandbox_id,
            client,
        })
    }

    /// Execute a shell command in the sandbox (POST /sandboxes/{id}/process)
    pub async fn exec(&self, command: &str, timeout_secs: u64) -> Result<String> {
        #[derive(Serialize)]
        struct ExecReq<'a> {
            cmd: &'a str,
            timeout: u64,
        }
        #[derive(Deserialize)]
        struct ExecResp {
            stdout: String,
            stderr: String,
            exit_code: i32,
        }

        let resp: ExecResp = self.client
            .post(format!("{}/sandboxes/{}/process", self.config.api_base, self.sandbox_id))
            .header("X-API-Key", &self.config.api_key)
            .json(&ExecReq { cmd: command, timeout: timeout_secs })
            .send()
            .await
            .map_err(|e| Error::Storage { message: format!("e2b exec: {e}") })?
            .json()
            .await
            .map_err(|e| Error::Storage { message: format!("e2b exec parse: {e}") })?;

        let output = if resp.stderr.is_empty() {
            resp.stdout
        } else {
            format!("{}\n[stderr]: {}", resp.stdout, resp.stderr)
        };
        Ok(output)
    }

    /// Upload a file to the sandbox (POST /sandboxes/{id}/files)
    pub async fn upload_file(&self, path: &str, content: &[u8]) -> Result<()> {
        use reqwest::multipart;
        let part = multipart::Part::bytes(content.to_vec()).file_name(path.to_string());
        let form = multipart::Form::new()
            .part("file", part)
            .text("path", path.to_string());

        self.client
            .post(format!("{}/sandboxes/{}/files", self.config.api_base, self.sandbox_id))
            .header("X-API-Key", &self.config.api_key)
            .multipart(form)
            .send()
            .await
            .map_err(|e| Error::Storage { message: format!("e2b upload: {e}") })?;
        Ok(())
    }

    /// Download a file from the sandbox (GET /sandboxes/{id}/files?path=...)
    pub async fn download_file(&self, path: &str) -> Result<Vec<u8>> {
        let bytes = self.client
            .get(format!("{}/sandboxes/{}/files", self.config.api_base, self.sandbox_id))
            .header("X-API-Key", &self.config.api_key)
            .query(&[("path", path)])
            .send()
            .await
            .map_err(|e| Error::Storage { message: format!("e2b download: {e}") })?
            .bytes()
            .await
            .map_err(|e| Error::Storage { message: format!("e2b download bytes: {e}") })?;
        Ok(bytes.to_vec())
    }

    /// Keep the sandbox alive (PATCH /sandboxes/{id})
    pub async fn refresh_timeout(&self) -> Result<()> {
        self.client
            .patch(format!("{}/sandboxes/{}", self.config.api_base, self.sandbox_id))
            .header("X-API-Key", &self.config.api_key)
            .json(&serde_json::json!({ "timeout": self.config.timeout_secs }))
            .send()
            .await
            .map_err(|e| Error::Storage { message: format!("e2b refresh: {e}") })?;
        Ok(())
    }
}

impl Drop for E2bSandbox {
    fn drop(&mut self) {
        let client = self.client.clone();
        let url = format!("{}/sandboxes/{}", self.config.api_base, self.sandbox_id);
        let api_key = self.config.api_key.clone();
        tokio::spawn(async move {
            let _ = client.delete(&url).header("X-API-Key", &api_key).send().await;
        });
    }
}

/// ToolSetProvider that routes shell commands to an E2B microVM.
pub struct E2bToolSetProvider {
    config: E2bConfig,
    sandbox: Arc<Mutex<Option<E2bSandbox>>>,
}

impl E2bToolSetProvider {
    pub fn new(config: E2bConfig) -> Self {
        Self { config, sandbox: Arc::new(Mutex::new(None)) }
    }
}

impl ToolSetProvider for E2bToolSetProvider {
    fn build_registry(&self) -> ToolRegistry {
        let mut registry = crate::tools::build_default_registry();
        // Register E2B shell execution as "run_shell" with sandbox backing.
        // The actual execution is handled by the E2bShellTool wrapper.
        registry.register_e2b_shell(self.config.clone(), Arc::clone(&self.sandbox));
        registry
    }
    fn sandbox_mode(&self) -> SandboxMode {
        SandboxMode::MicroVm
    }
}
```

### 3. 导出

```rust
#[cfg(feature = "e2b-sandbox")]
pub mod e2b_provider;
#[cfg(feature = "e2b-sandbox")]
pub use e2b_provider::{E2bConfig, E2bSandbox, E2bToolSetProvider};
```

### 4. 测试策略

- 单元测试仅在 `RECURSIVE_E2B_API_KEY` 环境变量存在时运行（需要真实 API key 或 mock server）
- 用 `wiremock` 或手写 mock HTTP server 做无 API key 的集成测试
- 测试：创建沙盒 → 执行 `echo hello` → 输出 "hello" → 删除沙盒

## Acceptance

- `cargo build --features e2b-sandbox` 编译通过
- `cargo test --workspace` 绿色（无 E2B key 时 skip）
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `E2bToolSetProvider` 实现 `ToolSetProvider`，`SandboxMode::MicroVm`
- 无 `e2b-sandbox` feature 时构建完全不受影响

## Notes for the agent

- E2B 官方目前没有 Rust SDK，本 Goal 直接对接 REST API（v0，可能有变化）。
  参考文档：https://e2b.dev/docs/api-reference
- `E2bSandbox::exec` 的 API 路径需根据 E2B 最新文档核对；本 Goal 的接口形状供参考，
  实际编译时按文档调整。
- Sandbox 复用策略：一个 `E2bToolSetProvider` 对应一个 session，复用同一个沙盒，
  避免每次 shell 调用都重建沙盒（极高延迟）。
- `Arc<Mutex<Option<E2bSandbox>>>` 实现懒初始化：首次调用时创建，后续复用。
- 文件同步策略：`write_file` 先写本地，再 `upload_file` 同步到沙盒；
  `run_shell` 的输出只在沙盒内，通过 `download_file` 拉回宿主机。
  这个双向同步逻辑在 `register_e2b_shell` 辅助函数中实现。
