//! E2B Firecracker microVM-backed [`ToolSetProvider`] (L3 sandbox, Goal 405).
//!
//! Each session lazily creates one E2B sandbox via the REST API. The whole
//! shared [`ToolTransport`] (`E2bTransport`) is rebound — every I/O tool
//! (Read / Write / Edit / Glob / Grep / count_lines / Bash) executes inside
//! the microVM (hardware-isolated, <150ms cold start); files move via the
//! E2B filesystem API. This mirrors the container tier
//! (`container_provider.rs`, Goal 403): transport + provider + builder
//! wiring, no branching in the agent loop.
//!
//! Sandbox creation failure is a hard error — the provider never silently
//! degrades to local execution.
//!
//! Gated behind the `e2b-sandbox` feature flag (default off).
//!
//! # Setup
//!
//! Set `RECURSIVE_E2B_API_KEY` before use. Optionally override:
//! - `RECURSIVE_E2B_TEMPLATE` (default: `"base"`)
//! - `RECURSIVE_E2B_TIMEOUT_SECS` (default: `3600`) — sandbox TTL; renewed
//!   after each successful exec once half of it has elapsed.
//! - `RECURSIVE_E2B_API_BASE` (default: `"https://api.e2b.dev"`)

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Mutex;

use crate::error::{Error, Result};
use crate::tool_set_provider::{SandboxMode, ToolSetProvider};
use crate::tools::transport::{
    DirEntry, EnvironmentCapabilities, ExecResult, ToolTransport, WalkEntry, WalkOptions,
};
use crate::tools::ToolRegistry;

// ─────────────────────────────────────────────────────────────────────────────
// E2bConfig
// ─────────────────────────────────────────────────────────────────────────────

/// Configuration for an E2B sandbox session. `Debug` deliberately omits
/// nothing — callers must never format it into logs (it carries the API
/// key); it exists only to satisfy `unwrap_err` in tests.
#[derive(Clone, Debug)]
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

/// Classify an HTTP status from the E2B REST API into an `io::ErrorKind`.
/// 404 → NotFound (lets the fs tools report "file not found" instead of an
/// opaque storage error); auth failures → PermissionDenied; anything else
/// is surfaced as-is.
fn status_to_io_error(status: reqwest::StatusCode) -> std::io::Error {
    let kind = match status.as_u16() {
        404 => std::io::ErrorKind::NotFound,
        401 | 403 => std::io::ErrorKind::PermissionDenied,
        408 | 429 => std::io::ErrorKind::TimedOut,
        _ => std::io::ErrorKind::Other,
    };
    std::io::Error::new(kind, format!("e2b HTTP {status}"))
}

/// Fixed root inside the VM that mirrors the host workspace (the container
/// tier bind-mounts the workspace here; the E2B tier instead starts empty
/// — see `E2bTransport::ensure_started`).
const VM_WORKSPACE_ROOT: &str = "/workspace";

/// Map a host-resolved workspace path to the in-VM absolute path.
///
/// The transport receives host-resolved paths whose prefix is the host
/// workspace (the tool layer resolves against the host workspace root);
/// the VM mirrors that tree under `VM_WORKSPACE_ROOT`. Paths outside the
/// workspace prefix are rejected — they cannot exist inside the VM.
/// Same contract as `ContainerTransport::map_path`.
fn map_path(workspace: &Path, path: &Path) -> std::io::Result<PathBuf> {
    let rel = path.strip_prefix(workspace).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "path {} escapes the e2b workspace mapping {}",
                path.display(),
                workspace.display()
            ),
        )
    })?;
    if rel.components().next().is_some() {
        Ok(PathBuf::from(VM_WORKSPACE_ROOT).join(rel))
    } else {
        Ok(PathBuf::from(VM_WORKSPACE_ROOT))
    }
}

/// Shell-quote a string for embedding in a command executed inside the VM.
fn shell_quote(s: &str) -> String {
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

// ─────────────────────────────────────────────────────────────────────────────
// E2bSandbox
// ─────────────────────────────────────────────────────────────────────────────

/// A live E2B sandbox session.
///
/// Created via `POST /sandboxes`; reclaimed via [`E2bSandbox::delete`]
/// (also fired best-effort from `Drop`).
pub struct E2bSandbox {
    config: E2bConfig,
    sandbox_id: String,
    client: reqwest::Client,
}

impl E2bSandbox {
    /// Create a new sandbox (POST /sandboxes).
    pub async fn create(config: E2bConfig) -> Result<Self> {
        // No global request timeout: per-call timeouts (exec 60/120s+
        // configurable, files API uploads/downloads of large trees) are
        // longer than any fixed bound; bound only the connect phase.
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| Error::Storage {
                message: format!("http client: {e}"),
            })?;

        #[derive(Serialize)]
        struct CreateReq {
            template_id: String,
            timeout: u32,
        }
        #[derive(Deserialize, Default)]
        #[serde(rename_all = "camelCase")]
        #[allow(non_snake_case)]
        struct CreateResp {
            #[serde(default)]
            sandbox_id: String,
            #[serde(default, rename = "sandboxID")]
            sandbox_id_legacy: String,
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
            .error_for_status()
            .map_err(|e| Error::Storage {
                message: format!("e2b create sandbox HTTP error: {e}"),
            })?
            .json()
            .await
            .map_err(|e| Error::Storage {
                message: format!("e2b create sandbox parse: {e}"),
            })?;

        let sandbox_id = if resp.sandbox_id.is_empty() {
            resp.sandbox_id_legacy
        } else {
            resp.sandbox_id
        };
        if sandbox_id.is_empty() {
            return Err(Error::Storage {
                message: "e2b create sandbox: response contained no sandbox id".into(),
            });
        }

        Ok(Self {
            config,
            sandbox_id,
            client,
        })
    }

    pub fn sandbox_id(&self) -> &str {
        &self.sandbox_id
    }

    /// Raw execution result of one command inside the sandbox
    /// (POST /sandboxes/{id}/process). Non-zero exits are NOT errors here —
    /// the caller (transport / tool layer) decides how to classify them.
    async fn exec_raw(&self, command: &str, timeout_secs: u64) -> Result<(i32, String, String)> {
        #[derive(Serialize)]
        struct ExecReq<'a> {
            cmds: [&'a str; 1],
            timeout: u64,
        }
        #[derive(Deserialize, Default)]
        struct RunResp {
            #[serde(default)]
            exit_code: i32,
            #[serde(default)]
            stdout: String,
            #[serde(default)]
            stderr: String,
        }
        #[derive(Deserialize)]
        struct ExecResp {
            run: Option<RunResp>,
            #[serde(flatten)]
            direct: RunResp,
        }

        let resp: ExecResp = self
            .client
            .post(format!(
                "{}/sandboxes/{}/process",
                self.config.api_base, self.sandbox_id
            ))
            .header("X-API-Key", &self.config.api_key)
            .json(&ExecReq {
                cmds: [command],
                timeout: timeout_secs,
            })
            .send()
            .await
            .map_err(|e| Error::Storage {
                message: format!("e2b exec: {e}"),
            })?
            .error_for_status()
            .map_err(|e| Error::Storage {
                message: format!("e2b exec HTTP error: {e}"),
            })?
            .json()
            .await
            .map_err(|e| Error::Storage {
                message: format!("e2b exec parse: {e}"),
            })?;

        // The legacy shape nests the result under `run`; accept the flat
        // shape too rather than guess (conservative parsing, no defaults
        // invented for data that decides success/failure).
        let r = resp.run.unwrap_or(resp.direct);
        Ok((r.exit_code, r.stdout, r.stderr))
    }

    /// Execute a shell command inside the sandbox and return combined
    /// output, erroring on non-zero exit (used by live smoke tests).
    pub async fn exec(&self, command: &str, timeout_secs: u64) -> Result<String> {
        let (code, stdout, stderr) = self.exec_raw(command, timeout_secs).await?;
        if code != 0 {
            return Err(Error::Tool {
                name: "Bash".into(),
                call_id: None,
                message: format!("command exited with code {code}: {stderr}"),
            });
        }
        let output = if stderr.is_empty() {
            stdout
        } else {
            format!("{stdout}\n[stderr]: {stderr}")
        };
        Ok(output)
    }

    /// Upload a file to the sandbox
    /// (POST /sandboxes/{id}/files?path=..., raw body = contents).
    pub async fn upload_file(&self, path: &str, content: &[u8]) -> std::io::Result<()> {
        let resp = self
            .client
            .post(format!(
                "{}/sandboxes/{}/files",
                self.config.api_base, self.sandbox_id
            ))
            .header("X-API-Key", &self.config.api_key)
            .query(&[("path", path)])
            .body(content.to_vec())
            .send()
            .await
            .map_err(|e| std::io::Error::other(format!("e2b upload_file: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let err_body = resp.text().await.unwrap_or_default();
            let mut err = status_to_io_error(status);
            err = std::io::Error::new(err.kind(), format!("{err}: {err_body}"));
            return Err(err);
        }
        Ok(())
    }

    /// Download a file from the sandbox (GET /sandboxes/{id}/files?path=...).
    /// 404 maps to [`std::io::ErrorKind::NotFound`].
    pub async fn download_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
        let response = self
            .client
            .get(format!(
                "{}/sandboxes/{}/files",
                self.config.api_base, self.sandbox_id
            ))
            .header("X-API-Key", &self.config.api_key)
            .query(&[("path", path)])
            .send()
            .await
            .map_err(|e| std::io::Error::other(format!("e2b download_file: {e}")))?;
        if !response.status().is_success() {
            let status = response.status();
            let err_body = response.text().await.unwrap_or_default();
            let mut err = status_to_io_error(status);
            err = std::io::Error::new(err.kind(), format!("{err}: {err_body}"));
            return Err(err);
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| std::io::Error::other(format!("e2b download_file bytes: {e}")))?;
        Ok(bytes.to_vec())
    }

    /// Extend the sandbox TTL (PATCH /sandboxes/{id}).
    pub async fn refresh_timeout(&self) -> Result<()> {
        let resp = self
            .client
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
        if !resp.status().is_success() {
            let status = resp.status();
            let err_body = resp.text().await.unwrap_or_default();
            return Err(Error::Storage {
                message: format!("e2b refresh_timeout HTTP {status}: {err_body}"),
            });
        }
        Ok(())
    }

    /// Destroy the sandbox (DELETE /sandboxes/{id}). Idempotent.
    pub async fn delete(&self) -> Result<()> {
        let resp = self
            .client
            .delete(format!(
                "{}/sandboxes/{}",
                self.config.api_base, self.sandbox_id
            ))
            .header("X-API-Key", &self.config.api_key)
            .send()
            .await
            .map_err(|e| Error::Storage {
                message: format!("e2b delete: {e}"),
            })?;
        if !resp.status().is_success() {
            let status = resp.status();
            // 404 = already gone — that IS success for an idempotent delete.
            if status.as_u16() != 404 {
                let err_body = resp.text().await.unwrap_or_default();
                return Err(Error::Storage {
                    message: format!("e2b delete HTTP {status}: {err_body}"),
                });
            }
        }
        Ok(())
    }
}

impl Drop for E2bSandbox {
    fn drop(&mut self) {
        if std::env::var("RECURSIVE_E2B_NO_DROP_DELETE").is_ok() {
            return;
        }
        let client = self.client.clone();
        let url = format!("{}/sandboxes/{}", self.config.api_base, self.sandbox_id);
        let api_key = self.config.api_key.clone();
        // Best-effort delete. tokio::spawn panics outside a runtime (e.g.
        // process teardown after the runtime shut down); mirror the
        // container tier — fall back to a blocking current-thread runtime.
        let spawned = tokio::runtime::Handle::try_current().map(|h| {
            let (client, url, api_key) = (client.clone(), url.clone(), api_key.clone());
            h.spawn(async move {
                let _ = client
                    .delete(&url)
                    .header("X-API-Key", &api_key)
                    .send()
                    .await;
            })
        });
        if spawned.is_err() {
            let _ = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map(|rt| {
                    rt.block_on(async move {
                        let _ = client
                            .delete(&url)
                            .header("X-API-Key", &api_key)
                            .send()
                            .await;
                    })
                });
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// E2bTransport
// ─────────────────────────────────────────────────────────────────────────────

/// Shared [`ToolTransport`] bound to one lazily-created E2B microVM.
///
/// The sandbox is created on the first I/O call (or eagerly via
/// [`E2bTransport::ensure_started`]) and reused for the transport's
/// lifetime. The VM workspace root (`/workspace`, created at sandbox
/// start) mirrors the host workspace: the tool layer passes host-resolved
/// paths, which [`map_path`] translates to in-VM paths before every
/// operation (`read_file`/`write_file`/`list_dir`/`walk`/
/// `create_dir_all`/`exec_shell` cwd). The VM starts **empty** — host
/// workspace contents are NOT pre-uploaded; the first Write/Edit in a
/// session creates the file inside the VM, and Read of a pre-existing
/// host file returns `NotFound` (by design: uploading whole trees per
/// session is out of scope for this tier). Capabilities (`path_root` =
/// `/workspace`, `user` via `whoami`, `toolchain` via `command -v`) are
/// probed once at sandbox startup and cached so the sync `capabilities()`
/// reports real data.
pub struct E2bTransport {
    config: E2bConfig,
    workspace: PathBuf,
    sandbox: Arc<Mutex<Option<E2bSandbox>>>,
    caps: std::sync::RwLock<EnvironmentCapabilities>,
    /// Monotonic clock of the last TTL renewal; `None` = never renewed.
    last_refresh: Mutex<Option<std::time::Instant>>,
    /// Set by `destroy()`; repeated calls are no-ops (idempotent contract).
    destroyed: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for E2bTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("E2bTransport")
            .field("template_id", &self.config.template_id)
            .field("timeout_secs", &self.config.timeout_secs)
            .finish_non_exhaustive()
    }
}

fn err_to_io(e: Error) -> std::io::Error {
    match e {
        // preserve kinds we already classified (e.g. NotFound from download)
        Error::Storage { message } => std::io::Error::other(message),
        other => std::io::Error::other(other.to_string()),
    }
}

fn default_caps() -> EnvironmentCapabilities {
    EnvironmentCapabilities {
        network: true,
        persistent: true,
        path_root: PathBuf::from(VM_WORKSPACE_ROOT),
        user: Some("root".into()),
        toolchain: Vec::new(),
        snapshot: false,
    }
}

impl E2bTransport {
    pub fn new(config: E2bConfig, workspace: impl Into<PathBuf>) -> Self {
        // Pre-probe defaults: path_root is the fixed VM workspace root;
        // user/toolchain are overwritten by the real probe at sandbox start.
        Self {
            config,
            workspace: workspace.into(),
            sandbox: Arc::new(Mutex::new(None)),
            caps: std::sync::RwLock::new(default_caps()),
            last_refresh: Mutex::new(None),
            destroyed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Create the sandbox (if needed) and probe capabilities.
    /// Safe to call repeatedly — the probe runs once per transport.
    pub async fn ensure_started(&self) -> Result<()> {
        let mut guard = self.sandbox.lock().await;
        if guard.is_some() {
            return Ok(());
        }
        let sandbox = E2bSandbox::create(self.config.clone()).await?;

        // Fixed VM workspace root — every host workspace path maps under
        // here (see `map_path`). Created eagerly so exec_shell's
        // `cd /workspace` works even before any file is written.
        let (code, _, stderr) = sandbox
            .exec_raw(&format!("mkdir -p {}", shell_quote(VM_WORKSPACE_ROOT)), 30)
            .await?;
        if code != 0 {
            return Err(Error::Storage {
                message: format!("e2b mkdir workspace root failed: {stderr}"),
            });
        }
        *guard = Some(sandbox);

        // Probe the environment (best-effort per field — a failed probe
        // leaves the conservative default rather than failing the session).
        let sb = match guard.as_ref() {
            Some(sb) => sb,
            None => return Ok(()), // unreachable: set two lines above
        };
        let who = probe_trim(sb, "whoami").await;
        let toolchain = probe_toolchain(sb).await;
        if let Ok(mut caps) = self.caps.write() {
            caps.path_root = PathBuf::from(VM_WORKSPACE_ROOT);
            if let Some(u) = who {
                caps.user = Some(u);
            }
            caps.toolchain = toolchain;
        }
        Ok(())
    }

    /// Renew the sandbox TTL if more than half of `timeout_secs` has passed
    /// since the last renewal (or since start). Failure is surfaced as an
    /// error, never silently swallowed (plan §A.2).
    async fn maybe_refresh_ttl(&self) -> Result<()> {
        let half = Duration::from_secs(u64::from(self.config.timeout_secs) / 2);
        let mut last = self.last_refresh.lock().await;
        let due = match *last {
            None => true,
            Some(t) => t.elapsed() >= half,
        };
        if !due {
            return Ok(());
        }
        let guard = self.sandbox.lock().await;
        if let Some(sb) = guard.as_ref() {
            sb.refresh_timeout().await?;
        }
        *last = Some(std::time::Instant::now());
        Ok(())
    }

    /// Run one command inside the VM, ensuring the sandbox exists.
    /// Returns the raw (exit_code, stdout, stderr) triple.
    async fn vm_exec(&self, command: &str, timeout: Duration) -> Result<(i32, String, String)> {
        self.ensure_started().await?;
        let guard = self.sandbox.lock().await;
        let sb = guard.as_ref().ok_or_else(|| Error::Storage {
            message: "e2b sandbox missing after ensure_started".into(),
        })?;
        let secs = timeout.as_secs().max(1);
        sb.exec_raw(command, secs).await
    }

    async fn vm_exec_io(
        &self,
        command: &str,
        timeout: Duration,
    ) -> std::io::Result<(i32, String, String)> {
        self.vm_exec(command, timeout).await.map_err(err_to_io)
    }
}

/// Run a probe command and return its trimmed stdout (`None` on failure).
async fn probe_trim(sandbox: &E2bSandbox, cmd: &str) -> Option<String> {
    let (code, stdout, _) = sandbox.exec_raw(cmd, 10).await.ok()?;
    if code != 0 {
        return None;
    }
    let t = stdout.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Probe which of the standard tools exist in the VM.
async fn probe_toolchain(sandbox: &E2bSandbox) -> Vec<String> {
    let cmd =
        "for t in cargo node rg git; do command -v \"$t\" >/dev/null 2>&1 && echo \"$t\"; done";
    let (code, stdout, _) =
        sandbox
            .exec_raw(cmd, 10)
            .await
            .unwrap_or((1, String::new(), String::new()));
    if code != 0 {
        return Vec::new();
    }
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Parse one `%y\t%f` line from `find -maxdepth 1` into a [`DirEntry`].
fn parse_ls_line(line: &str) -> Option<DirEntry> {
    let (kind, name) = line.split_once('\t')?;
    Some(DirEntry {
        name: name.to_string(),
        is_dir: kind == "d",
    })
}

/// Parse one `%y\t%p\t%s` line from `find` into a [`WalkEntry`] relative to
/// `root`. Unparsable / non-relative lines are skipped (caller filters).
fn parse_walk_line(line: &str, root: &str) -> Option<WalkEntry> {
    let (kind, rest) = line.split_once('\t')?;
    let (path, size) = rest.rsplit_once('\t')?;
    let rel = path.strip_prefix(root.trim_end_matches('/'))?;
    let rel = rel.trim_start_matches('/');
    if rel.is_empty() {
        return None; // the root itself is excluded (walk contract)
    }
    Some(WalkEntry {
        path: PathBuf::from(rel),
        is_file: kind == "f",
        size: size.parse().ok()?,
    })
}

fn cap_string(s: &mut String, max: usize) {
    if s.len() > max {
        // char-boundary-safe truncation
        let mut cut = max;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push_str("\n... [output truncated]");
    }
}

#[async_trait]
impl ToolTransport for E2bTransport {
    fn capabilities(&self) -> EnvironmentCapabilities {
        self.caps
            .read()
            .map(|c| c.clone())
            // poisoned lock: report the conservative default rather than
            // panic (invariant #5 — no panics in non-test code)
            .unwrap_or_else(|_| default_caps())
    }

    async fn destroy(&self) {
        if self
            .destroyed
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }
        let mut guard = self.sandbox.lock().await;
        if let Some(sb) = guard.take() {
            if let Err(e) = sb.delete().await {
                // destroy never resurrects a session — log only.
                eprintln!("recursive: e2b destroy failed: {e}");
            }
        }
    }

    async fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        let vm_path = map_path(&self.workspace, path)?;
        self.ensure_started().await.map_err(err_to_io)?;
        let guard = self.sandbox.lock().await;
        let sb = guard
            .as_ref()
            .ok_or_else(|| std::io::Error::other("e2b sandbox missing"))?;
        sb.download_file(&vm_path.to_string_lossy()).await
    }

    async fn write_file(&self, path: &Path, contents: &[u8]) -> std::io::Result<()> {
        let vm_path = map_path(&self.workspace, path)?;
        self.ensure_started().await.map_err(err_to_io)?;
        if let Some(parent) = vm_path.parent() {
            let cmd = format!("mkdir -p {}", shell_quote(&parent.to_string_lossy()));
            let (code, _, stderr) = self.vm_exec_io(&cmd, Duration::from_secs(30)).await?;
            if code != 0 {
                return Err(std::io::Error::other(format!(
                    "mkdir failed in e2b vm: {stderr}"
                )));
            }
        }
        let guard = self.sandbox.lock().await;
        let sb = guard
            .as_ref()
            .ok_or_else(|| std::io::Error::other("e2b sandbox missing"))?;
        sb.upload_file(&vm_path.to_string_lossy(), contents).await
    }

    async fn list_dir(&self, path: &Path) -> std::io::Result<Vec<DirEntry>> {
        let vm_path = map_path(&self.workspace, path)?;
        let cmd = format!(
            "find {} -mindepth 1 -maxdepth 1 -printf '%y\\t%f\\n'",
            shell_quote(&vm_path.to_string_lossy())
        );
        let (code, stdout, stderr) = self.vm_exec_io(&cmd, Duration::from_secs(60)).await?;
        if code != 0 || stderr.contains("No such file or directory") {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("dir not found in e2b vm: {}: {stderr}", path.display()),
            ));
        }
        Ok(stdout.lines().filter_map(parse_ls_line).collect())
    }

    /// Single-round-trip walk inside the VM (must NOT use the O(dirs)
    /// `list_dir` fallback — see the trait docs).
    async fn walk(&self, root: &Path, opts: &WalkOptions) -> std::io::Result<Vec<WalkEntry>> {
        let vm_root = map_path(&self.workspace, root)?;
        let root_str = vm_root.to_string_lossy().into_owned();
        // follow_symlinks: unsound across the VM boundary and unsupported
        // by the base template policy — degrade to the non-following walk
        // (same policy as the container tier).
        let prunes: Vec<String> = opts
            .ignore_dirs
            .iter()
            .map(|d| format!("-name {}", shell_quote(d)))
            .collect();
        let prune = if prunes.is_empty() {
            String::new()
        } else {
            format!("\\( {} \\) -prune ", prunes.join(" -o "))
        };
        let max_depth = if opts.max_depth == usize::MAX {
            String::new()
        } else {
            format!("-maxdepth {} ", opts.max_depth)
        };
        let action = if prunes.is_empty() {
            "-printf '%y\\t%p\\t%s\\n'"
        } else {
            "-o -printf '%y\\t%p\\t%s\\n'"
        };
        let cmd = format!(
            "find {} {}{}{}",
            shell_quote(&root_str),
            max_depth,
            prune,
            action
        );
        let (code, stdout, stderr) = self.vm_exec_io(&cmd, Duration::from_secs(120)).await?;
        if code != 0 || stderr.contains("No such file or directory") {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("walk root not found in e2b vm: {}", root.display()),
            ));
        }
        let mut out: Vec<WalkEntry> = stdout
            .lines()
            .filter_map(|l| parse_walk_line(l, &root_str))
            .filter(|e| {
                !e.path.components().any(|c| {
                    c.as_os_str()
                        .to_str()
                        .map(|n| opts.ignore_dirs.iter().any(|i| i == n))
                        .unwrap_or(false)
                })
            })
            .collect();
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    async fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
        let vm_path = map_path(&self.workspace, path)?;
        let cmd = format!("mkdir -p {}", shell_quote(&vm_path.to_string_lossy()));
        let (code, _, stderr) = self.vm_exec_io(&cmd, Duration::from_secs(30)).await?;
        if code != 0 {
            return Err(std::io::Error::other(format!(
                "mkdir failed in e2b vm: {stderr}"
            )));
        }
        Ok(())
    }

    async fn exec_shell(
        &self,
        command: &str,
        cwd: &Path,
        env: &[(String, String)],
        timeout: Duration,
        max_output_bytes: usize,
    ) -> std::io::Result<ExecResult> {
        let vm_cwd = map_path(&self.workspace, cwd)?;
        let mut env_prefix = String::new();
        for (k, v) in env {
            env_prefix.push_str(&format!("{}={} ", k, shell_quote(v)));
        }
        let script = format!(
            "cd {} && {}sh -c {}",
            shell_quote(&vm_cwd.to_string_lossy()),
            env_prefix,
            shell_quote(command),
        );
        let (code, mut stdout, mut stderr) = self.vm_exec_io(&script, timeout).await?;
        cap_string(&mut stdout, max_output_bytes);
        cap_string(&mut stderr, max_output_bytes);
        let result = ExecResult {
            exit_code: Some(code),
            stdout,
            stderr,
            failure: None,
        };
        // TTL renewal after a successful transport call; a failed renewal
        // is an error, never silent (the sandbox may expire mid-session).
        if code == 0 {
            self.maybe_refresh_ttl().await.map_err(err_to_io)?;
        }
        Ok(result)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// E2bToolSetProvider
// ─────────────────────────────────────────────────────────────────────────────

/// [`ToolSetProvider`] whose entire tool set executes inside an E2B microVM.
///
/// Mirrors the container tier: one shared [`E2bTransport`] is handed to
/// [`crate::tools::build_standard_tools_with_transport_opt`] so every I/O
/// tool follows the VM while all non-transport tools (memory / facts /
/// TodoWrite / web / LoadSkill …) survive intact. Sandbox creation failure
/// aborts the process — never a silent local fallback.
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
    pub fn from_env_provider(
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

    /// Create the transport eagerly (sandbox + capability probe).
    /// Result-returning variant for callers that prefer `Result` over the
    /// fatal `build_registry` contract.
    pub async fn create_transport(&self) -> Result<E2bTransport> {
        let t = E2bTransport::new(self.config.clone(), self.workspace.clone());
        t.ensure_started().await?;
        Ok(t)
    }
}

impl ToolSetProvider for E2bToolSetProvider {
    fn build_registry(&self) -> ToolRegistry {
        // Same pattern as the container tier: `build_registry` is sync but
        // sandbox creation is async (block_in_place needs the multi-thread
        // runtime the CLI already runs). Creation failure aborts with a
        // clear message — silently degrading to host execution would
        // defeat the sandbox.
        let transport = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async { self.create_transport().await })
        })
        .unwrap_or_else(|e| {
            eprintln!(
                "recursive: e2b microVM sandbox creation failed: {e} \
                 (refusing to fall back to local execution)"
            );
            std::process::exit(2);
        });
        let shared: Arc<dyn ToolTransport> = Arc::new(transport);

        crate::tools::build_standard_tools_with_transport_opt(
            shared,
            &self.workspace,
            &[],
            None,
            &self.skills,
            self.shell_timeout_secs,
            None,
            None,
            None,
            None,
            // run_background / watch_file execute via the HOST /bin/sh, so
            // they are dropped rather than silently exposing host execution
            // (same honest-degradation policy as the container tier).
            true,
        )
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

    #[test]
    fn e2b_provider_sandbox_mode_is_microvm() {
        let config = E2bConfig {
            api_key: "test-key".into(),
            template_id: "base".into(),
            timeout_secs: 60,
            api_base: "https://api.e2b.dev".into(),
        };
        let p = E2bToolSetProvider::new(config, PathBuf::from("/tmp"), 30, vec![]);
        assert_eq!(p.sandbox_mode(), SandboxMode::MicroVm);
    }

    #[test]
    fn e2b_config_from_env_defaults_and_overrides() {
        // Save + clear, set explicitly, restore — self-contained despite
        // the global env (these tests are the only readers of these vars).
        let keys = [
            "RECURSIVE_E2B_API_KEY",
            "RECURSIVE_E2B_TEMPLATE",
            "RECURSIVE_E2B_TIMEOUT_SECS",
            "RECURSIVE_E2B_API_BASE",
        ];
        let saved: Vec<Option<String>> = keys.iter().map(|k| std::env::var(k).ok()).collect();
        for k in keys {
            std::env::remove_var(k);
        }

        // Missing key → Config error.
        let err = E2bConfig::from_env().unwrap_err();
        assert!(err.to_string().contains("RECURSIVE_E2B_API_KEY"));

        std::env::set_var("RECURSIVE_E2B_API_KEY", "k1");
        let c = E2bConfig::from_env().unwrap();
        assert_eq!(c.api_key, "k1");
        assert_eq!(c.template_id, "base", "template default");
        assert_eq!(c.timeout_secs, 3600, "timeout default");
        assert_eq!(c.api_base, "https://api.e2b.dev", "api base default");

        std::env::set_var("RECURSIVE_E2B_TEMPLATE", "custom");
        std::env::set_var("RECURSIVE_E2B_TIMEOUT_SECS", "120");
        std::env::set_var("RECURSIVE_E2B_API_BASE", "http://localhost:9999");
        let c = E2bConfig::from_env().unwrap();
        assert_eq!(c.template_id, "custom");
        assert_eq!(c.timeout_secs, 120);
        assert_eq!(c.api_base, "http://localhost:9999");

        // Non-numeric timeout falls back to the default (documented).
        std::env::set_var("RECURSIVE_E2B_TIMEOUT_SECS", "not-a-number");
        let c = E2bConfig::from_env().unwrap();
        assert_eq!(c.timeout_secs, 3600);

        for (k, v) in keys.iter().zip(saved.iter()) {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
    }

    #[test]
    fn http_status_classification() {
        let e = status_to_io_error(reqwest::StatusCode::NOT_FOUND);
        assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
        let e = status_to_io_error(reqwest::StatusCode::UNAUTHORIZED);
        assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
        let e = status_to_io_error(reqwest::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(e.kind(), std::io::ErrorKind::TimedOut);
        let e = status_to_io_error(reqwest::StatusCode::BAD_GATEWAY);
        assert_eq!(e.kind(), std::io::ErrorKind::Other);
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("plain"), "'plain'");
    }

    #[test]
    fn parse_ls_line_distinguishes_dirs() {
        let d = parse_ls_line("d\tsub").unwrap();
        assert!(d.is_dir);
        assert_eq!(d.name, "sub");
        let f = parse_ls_line("f\ta.txt").unwrap();
        assert!(!f.is_dir);
        assert_eq!(f.name, "a.txt");
        assert!(parse_ls_line("garbage").is_none());
    }

    #[test]
    fn parse_walk_line_is_root_relative() {
        let e = parse_walk_line("f\t/root/src/b.rs\t12", "/root").unwrap();
        assert_eq!(e.path, PathBuf::from("src/b.rs"));
        assert!(e.is_file);
        assert_eq!(e.size, 12);
        // root itself excluded; unparsable size skipped.
        assert!(parse_walk_line("d\t/root\t4096", "/root").is_none());
        assert!(parse_walk_line("f\t/root/x\tbig", "/root").is_none());
        // path outside root prefix → skipped (conservative, no guessing).
        assert!(parse_walk_line("f\t/elsewhere/x\t1", "/root").is_none());
    }

    #[test]
    fn map_path_translates_workspace_prefix() {
        let ws = Path::new("/Users/kong/proj");
        assert_eq!(
            map_path(ws, Path::new("/Users/kong/proj")).unwrap(),
            PathBuf::from("/workspace")
        );
        assert_eq!(
            map_path(ws, Path::new("/Users/kong/proj/src/main.rs")).unwrap(),
            PathBuf::from("/workspace/src/main.rs")
        );
        let err = map_path(ws, Path::new("/etc/passwd")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn cap_string_truncates_with_marker() {
        let mut s = "x".repeat(100);
        cap_string(&mut s, 10);
        assert!(s.starts_with("xxxxxxxxxx"));
        assert!(s.ends_with("[output truncated]"));
        let mut short = String::from("abc");
        cap_string(&mut short, 10);
        assert_eq!(short, "abc");
    }

    #[test]
    fn e2b_transport_default_capabilities_are_conservative() {
        let config = E2bConfig {
            api_key: "k".into(),
            template_id: "base".into(),
            timeout_secs: 60,
            api_base: "https://api.e2b.dev".into(),
        };
        let t = E2bTransport::new(config, "/host/ws");
        let caps = t.capabilities();
        assert!(
            caps.network,
            "base template has outbound network by default"
        );
        assert!(caps.persistent);
        assert!(!caps.snapshot, "snapshot/clone is a documented non-goal");
        assert_eq!(caps.path_root, PathBuf::from("/workspace"));
    }

    #[test]
    fn e2b_transport_debug_has_no_secrets() {
        let config = E2bConfig {
            api_key: "secret-key-value".into(),
            template_id: "base".into(),
            timeout_secs: 60,
            api_base: "https://api.e2b.dev".into(),
        };
        let dbg = format!("{:?}", E2bTransport::new(config, "/host/ws"));
        assert!(
            !dbg.contains("secret-key-value"),
            "Debug must not leak the API key"
        );
    }

    /// Live round-trip against the real E2B API. Gated by
    /// `RECURSIVE_TEST_E2B_API_KEY` (skip when absent).
    #[tokio::test]
    async fn e2b_shell_exec_runs_against_live_api() {
        let Ok(key) = std::env::var("RECURSIVE_TEST_E2B_API_KEY") else {
            return; // skip when no E2B credentials available
        };
        let config = E2bConfig {
            api_key: key,
            template_id: std::env::var("RECURSIVE_E2B_TEMPLATE").unwrap_or_else(|_| "base".into()),
            timeout_secs: 300,
            api_base: std::env::var("RECURSIVE_E2B_API_BASE")
                .unwrap_or_else(|_| "https://api.e2b.dev".into()),
        };
        let transport = E2bTransport::new(config, "/host/ws");
        transport
            .ensure_started()
            .await
            .expect("create sandbox + probe");
        let r = transport
            .exec_shell(
                "echo hello",
                Path::new("/host/ws"),
                &[],
                Duration::from_secs(30),
                128 * 1024,
            )
            .await
            .expect("exec via transport");
        assert_eq!(r.exit_code, Some(0));
        assert!(r.stdout.contains("hello"), "stdout: {}", r.stdout);
        let caps = transport.capabilities();
        assert_eq!(
            caps.path_root,
            PathBuf::from("/workspace"),
            "path_root is the fixed VM workspace root"
        );
        assert!(caps.user.is_some(), "whoami probe filled user");
        transport.destroy().await;
    }
}
