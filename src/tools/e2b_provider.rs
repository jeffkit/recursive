//! E2B Firecracker microVM-backed [`ToolSetProvider`] (L3 sandbox).
//!
//! Each session creates an isolated E2B sandbox via the REST API. Commands
//! execute inside the microVM (hardware-isolated, <150ms cold start);
//! files are synced via the filesystem API.
//!
//! Gated behind the `e2b-sandbox` feature flag.
//!
//! # Setup
//!
//! Set `RECURSIVE_E2B_API_KEY` before use. Optionally override:
//! - `RECURSIVE_E2B_TEMPLATE` (default: `"base"`)
//! - `RECURSIVE_E2B_TIMEOUT_SECS` (default: `3600`)
//! - `RECURSIVE_E2B_API_BASE` (default: `"https://api.e2b.dev"`)

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::tool_set_provider::{SandboxMode, ToolSetProvider};
use crate::tools::{Tool, ToolRegistry, ToolSideEffect};

// ─────────────────────────────────────────────────────────────────────────────
// E2bConfig
// ─────────────────────────────────────────────────────────────────────────────

/// Configuration for an E2B sandbox session.
#[derive(Clone)]
pub struct E2bConfig {
    /// E2B API key.
    pub api_key: String,
    /// Sandbox template ID (default: `"base"`).
    pub template_id: String,
    /// Sandbox lifetime in seconds (default: 3600).
    pub timeout_secs: u32,
    /// E2B API base URL.
    pub api_base: String,
}

impl E2bConfig {
    /// Load configuration from environment variables.
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            api_key: std::env::var("RECURSIVE_E2B_API_KEY").map_err(|_| Error::Config {
                message: "RECURSIVE_E2B_API_KEY not set".into(),
            })?,
            template_id: std::env::var("RECURSIVE_E2B_TEMPLATE").unwrap_or_else(|_| "base".into()),
            timeout_secs: std::env::var("RECURSIVE_E2B_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(3600),
            api_base: std::env::var("RECURSIVE_E2B_API_BASE")
                .unwrap_or_else(|_| "https://api.e2b.dev".into()),
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// E2bSandbox
// ─────────────────────────────────────────────────────────────────────────────

/// A live E2B sandbox session.
///
/// Created via `POST /sandboxes` and automatically destroyed on `Drop`.
pub struct E2bSandbox {
    config: E2bConfig,
    sandbox_id: String,
    client: reqwest::Client,
}

impl E2bSandbox {
    /// Create a new sandbox (POST /sandboxes).
    pub async fn create(config: E2bConfig) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| Error::Storage {
                message: format!("http client: {e}"),
            })?;

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
            .map_err(|e| Error::Storage {
                message: format!("e2b create sandbox: {e}"),
            })?
            .json()
            .await
            .map_err(|e| Error::Storage {
                message: format!("e2b create sandbox parse: {e}"),
            })?;

        Ok(Self {
            config,
            sandbox_id: resp.sandbox_id,
            client,
        })
    }

    /// Execute a shell command inside the sandbox (POST /sandboxes/{id}/process).
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
            #[allow(dead_code)]
            exit_code: i32,
        }

        let resp: ExecResp = self
            .client
            .post(format!(
                "{}/sandboxes/{}/process",
                self.config.api_base, self.sandbox_id
            ))
            .header("X-API-Key", &self.config.api_key)
            .json(&ExecReq {
                cmd: command,
                timeout: timeout_secs,
            })
            .send()
            .await
            .map_err(|e| Error::Storage {
                message: format!("e2b exec: {e}"),
            })?
            .json()
            .await
            .map_err(|e| Error::Storage {
                message: format!("e2b exec parse: {e}"),
            })?;

        let output = if resp.stderr.is_empty() {
            resp.stdout
        } else {
            format!("{}\n[stderr]: {}", resp.stdout, resp.stderr)
        };
        Ok(output)
    }

    /// Upload a file to the sandbox.
    ///
    /// Sends the raw bytes as the request body with the path encoded as a
    /// query parameter (compatible with E2B's file upload endpoint).
    pub async fn upload_file(&self, path: &str, content: &[u8]) -> Result<()> {
        self.client
            .post(format!(
                "{}/sandboxes/{}/files",
                self.config.api_base, self.sandbox_id
            ))
            .header("X-API-Key", &self.config.api_key)
            .query(&[("path", path)])
            .body(content.to_vec())
            .send()
            .await
            .map_err(|e| Error::Storage {
                message: format!("e2b upload_file: {e}"),
            })?;
        Ok(())
    }

    /// Download a file from the sandbox (GET /sandboxes/{id}/files?path=...).
    pub async fn download_file(&self, path: &str) -> Result<Vec<u8>> {
        let bytes = self
            .client
            .get(format!(
                "{}/sandboxes/{}/files",
                self.config.api_base, self.sandbox_id
            ))
            .header("X-API-Key", &self.config.api_key)
            .query(&[("path", path)])
            .send()
            .await
            .map_err(|e| Error::Storage {
                message: format!("e2b download_file: {e}"),
            })?
            .bytes()
            .await
            .map_err(|e| Error::Storage {
                message: format!("e2b download_file bytes: {e}"),
            })?;
        Ok(bytes.to_vec())
    }

    /// Extend the sandbox TTL (PATCH /sandboxes/{id}).
    pub async fn refresh_timeout(&self) -> Result<()> {
        self.client
            .patch(format!(
                "{}/sandboxes/{}",
                self.config.api_base, self.sandbox_id
            ))
            .header("X-API-Key", &self.config.api_key)
            .json(&json!({ "timeout": self.config.timeout_secs }))
            .send()
            .await
            .map_err(|e| Error::Storage {
                message: format!("e2b refresh_timeout: {e}"),
            })?;
        Ok(())
    }
}

impl Drop for E2bSandbox {
    fn drop(&mut self) {
        let client = self.client.clone();
        let url = format!("{}/sandboxes/{}", self.config.api_base, self.sandbox_id);
        let api_key = self.config.api_key.clone();
        tokio::spawn(async move {
            let _ = client
                .delete(&url)
                .header("X-API-Key", &api_key)
                .send()
                .await;
        });
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// E2bShellTool
// ─────────────────────────────────────────────────────────────────────────────

/// Tool implementation that routes `run_shell` calls to an E2B microVM.
///
/// The sandbox is lazily initialised on the first `execute` call and reused
/// for the lifetime of the tool (i.e. the session).
pub struct E2bShellTool {
    config: E2bConfig,
    sandbox: Arc<Mutex<Option<E2bSandbox>>>,
    timeout_secs: u64,
}

impl E2bShellTool {
    pub fn new(config: E2bConfig, timeout_secs: u64) -> Self {
        Self {
            config,
            sandbox: Arc::new(Mutex::new(None)),
            timeout_secs,
        }
    }
}

#[async_trait]
impl Tool for E2bShellTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Bash".into(),
            description: "Run a shell command inside an isolated E2B microVM (Firecracker).".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell command to execute inside the E2B microVM"
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

        let mut guard = self.sandbox.lock().await;
        if guard.is_none() {
            let sandbox =
                E2bSandbox::create(self.config.clone())
                    .await
                    .map_err(|e| Error::Tool {
                        name: "Bash".into(),
                        message: format!("e2b sandbox init failed: {e}"),
                    })?;
            *guard = Some(sandbox);
        }
        let sandbox = guard.as_ref().unwrap();
        sandbox.exec(command, self.timeout_secs).await
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// E2bToolSetProvider
// ─────────────────────────────────────────────────────────────────────────────

/// [`ToolSetProvider`] that routes shell execution to an E2B microVM.
///
/// The sandbox is lazily created on the first `run_shell` call.
pub struct E2bToolSetProvider {
    config: E2bConfig,
    workspace: std::path::PathBuf,
    shell_timeout_secs: u64,
    skills: Vec<crate::skills::Skill>,
}

impl E2bToolSetProvider {
    pub fn new(
        config: E2bConfig,
        workspace: std::path::PathBuf,
        shell_timeout_secs: u64,
        skills: Vec<crate::skills::Skill>,
    ) -> Self {
        Self {
            config,
            workspace,
            shell_timeout_secs,
            skills,
        }
    }

    /// Create using `E2bConfig::from_env()`.
    pub fn from_env(
        workspace: std::path::PathBuf,
        shell_timeout_secs: u64,
        skills: Vec<crate::skills::Skill>,
    ) -> Result<Self> {
        Ok(Self::new(
            E2bConfig::from_env()?,
            workspace,
            shell_timeout_secs,
            skills,
        ))
    }
}

impl ToolSetProvider for E2bToolSetProvider {
    fn build_registry(&self) -> ToolRegistry {
        let mut registry = crate::tools::build_standard_tools(
            &self.workspace,
            &self.skills,
            self.shell_timeout_secs,
        );
        // Replace run_shell with the E2B-backed implementation.
        let e2b_shell = E2bShellTool::new(self.config.clone(), self.shell_timeout_secs);
        registry.register_mut(Arc::new(e2b_shell));
        registry
    }

    fn sandbox_mode(&self) -> SandboxMode {
        SandboxMode::MicroVm
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::time::timeout;

    #[test]
    fn e2b_provider_sandbox_mode_is_microvm() {
        let dir = TempDir::new().unwrap();
        let config = E2bConfig {
            api_key: "test-key".into(),
            template_id: "base".into(),
            timeout_secs: 60,
            api_base: "https://api.e2b.dev".into(),
        };
        let p = E2bToolSetProvider::new(config, dir.path().to_path_buf(), 30, vec![]);
        assert_eq!(p.sandbox_mode(), SandboxMode::MicroVm);
    }

    #[tokio::test]
    async fn e2b_shell_exec_runs_against_live_api() {
        if std::env::var("RECURSIVE_E2B_API_KEY").is_err() {
            return; // skip when no E2B credentials available
        }
        let config = E2bConfig::from_env().expect("valid E2B config");
        let sandbox = timeout(Duration::from_secs(30), E2bSandbox::create(config))
            .await
            .expect("timeout")
            .expect("create sandbox");

        let out = timeout(Duration::from_secs(10), sandbox.exec("echo hello", 10))
            .await
            .expect("timeout")
            .expect("exec");
        assert!(out.contains("hello"), "expected 'hello' in: {out}");
    }
}
