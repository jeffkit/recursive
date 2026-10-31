//! E2B Firecracker microVM-backed [`ToolSetProvider`] (L3 sandbox, Goal 405).
//!
//! Sandbox lifecycle is **per provider / per process**: each
//! `E2bToolSetProvider` / `E2bTransport` holds one sandbox in an
//! `Arc<Mutex<Option<E2bSandbox>>>`, lazily created via the REST API and
//! reused until `destroy()`. For a CLI single run the process ≈ the
//! session, but under `recursive http` all sessions share one VM
//! (`single-tenant`) — see the deployment warning in
//! `docs/architecture/execution-environments.md`. The whole
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
//!
//! # Egress gate (issue #51)
//!
//! E2B is a managed service — the host cannot toggle the VM NIC. With the
//! default `base` template (outbound network on), `from_env` refuses to
//! start unless `RECURSIVE_SANDBOX_NETWORK=on` is set (the same opt-in
//! switch the container tier uses). Point `RECURSIVE_E2B_TEMPLATE` at a
//! self-built no-egress template to bypass the gate; the template author
//! then owns the egress decision.

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
        let config = Self {
            api_key: std::env::var("RECURSIVE_E2B_API_KEY").map_err(|_| Error::Config {
                message: "RECURSIVE_E2B_API_KEY not set".into(),
            })?,
            template_id: std::env::var("RECURSIVE_E2B_TEMPLATE").unwrap_or_else(|_| "base".into()),
            // Issue #220: a present-but-bad timeout is a startup error naming
            // the var and raw value (the parse_env paradigm), not a silent 3600.
            timeout_secs: crate::config::parse_env("RECURSIVE_E2B_TIMEOUT_SECS", 3600u32)?,
            api_base: std::env::var("RECURSIVE_E2B_API_BASE")
                .unwrap_or_else(|_| "https://api.e2b.dev".into()),
        };
        // Host-side startup gate (issue #51): the default `base` template
        // ships with outbound network. E2B is a managed service so the host
        // cannot turn the NIC off — the only faithful "default-off egress"
        // semantics is refusing to start unless the user explicitly opts in
        // via RECURSIVE_SANDBOX_NETWORK=on (same switch as the container
        // tier) or brings their own no-egress template.
        if config.template_id == "base"
            && std::env::var("RECURSIVE_SANDBOX_NETWORK").as_deref() != Ok("on")
        {
            return Err(Error::Config {
                message: "microvm tier uses the 'base' template which allows outbound network; \
                    set RECURSIVE_SANDBOX_NETWORK=on to acknowledge, or point \
                    RECURSIVE_E2B_TEMPLATE at a no-egress custom template"
                    .into(),
            });
        }
        Ok(config)
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

fn default_caps(template_id: &str) -> EnvironmentCapabilities {
    EnvironmentCapabilities {
        // Honest reporting (issue #51): with the `base` template the host-side
        // gate only lets this transport exist when the user opted in; with a
        // custom template the template author owns the egress decision.
        network: std::env::var("RECURSIVE_SANDBOX_NETWORK").as_deref() == Ok("on")
            || template_id != "base",
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
        let caps = default_caps(&config.template_id);
        Self {
            config,
            workspace: workspace.into(),
            sandbox: Arc::new(Mutex::new(None)),
            caps: std::sync::RwLock::new(caps),
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
            .unwrap_or_else(|_| default_caps(&self.config.template_id))
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
    use std::io::{Read, Write};

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
            "RECURSIVE_SANDBOX_NETWORK",
        ];
        let saved: Vec<Option<String>> = keys.iter().map(|k| std::env::var(k).ok()).collect();
        for k in keys {
            std::env::remove_var(k);
        }
        // base template default passes the egress gate only with opt-in;
        // the defaults test exercises it with the gate acknowledged.
        std::env::set_var("RECURSIVE_SANDBOX_NETWORK", "on");

        // Missing key → Config error.
        let err = E2bConfig::from_env().unwrap_err();
        assert!(err.to_string().contains("RECURSIVE_E2B_API_KEY"));

        // Egress gate (issue #51): base template without opt-in → refuse;
        // with opt-in → ok; custom template → gate does not apply.
        std::env::set_var("RECURSIVE_E2B_API_KEY", "k1");
        std::env::remove_var("RECURSIVE_SANDBOX_NETWORK");
        let err = E2bConfig::from_env().unwrap_err();
        assert!(err.to_string().contains("RECURSIVE_SANDBOX_NETWORK=on"));
        // base + not opted in → capabilities honestly report network=false
        let t = E2bTransport::new(
            E2bConfig {
                api_key: "k1".into(),
                template_id: "base".into(),
                timeout_secs: 60,
                api_base: "https://api.e2b.dev".into(),
            },
            "/host/ws",
        );
        assert!(
            !t.capabilities().network,
            "base + no opt-in: host gate blocks startup, so report network=false"
        );
        std::env::set_var("RECURSIVE_SANDBOX_NETWORK", "on");
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

        // custom template bypasses the gate even without the opt-in
        // (the template author owns the egress decision).
        std::env::remove_var("RECURSIVE_SANDBOX_NETWORK");
        let c = E2bConfig::from_env().unwrap();
        assert_eq!(c.template_id, "custom");
        std::env::set_var("RECURSIVE_SANDBOX_NETWORK", "on");

        // Non-numeric timeout is a startup error naming the var and the raw
        // value (issue #220 — a silent fallback to 3600 hid the typo).
        std::env::set_var("RECURSIVE_E2B_TIMEOUT_SECS", "not-a-number");
        match E2bConfig::from_env() {
            Err(Error::Config { message }) => {
                assert!(
                    message.contains("RECURSIVE_E2B_TIMEOUT_SECS"),
                    "error must name the var, got: {message}"
                );
                assert!(
                    message.contains("not-a-number"),
                    "error must carry the raw value, got: {message}"
                );
            }
            other => panic!("expected a Config error for the bad timeout, got {other:?}"),
        }

        // base + opted in → capabilities report network=true.
        std::env::remove_var("RECURSIVE_E2B_TEMPLATE");
        let t = E2bTransport::new(
            E2bConfig {
                api_key: "k1".into(),
                template_id: "base".into(),
                timeout_secs: 60,
                api_base: "https://api.e2b.dev".into(),
            },
            "/host/ws",
        );
        assert!(
            t.capabilities().network,
            "base + RECURSIVE_SANDBOX_NETWORK=on: egress acknowledged"
        );
        assert!(t.capabilities().persistent);
        assert!(
            !t.capabilities().snapshot,
            "snapshot/clone is a documented non-goal"
        );
        assert_eq!(t.capabilities().path_root, PathBuf::from("/workspace"));

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

    // ── loopback mock of the E2B REST surface ────────────────────────────
    //
    // The sandbox client talks plain HTTP to `api_base`, so a std
    // TcpListener on 127.0.0.1 standing in for api.e2b.dev drives every
    // request shape and status-code branch without credentials.

    /// One recorded request: method, target (path + query), lowercased
    /// header names and the raw body.
    struct MockReq {
        method: String,
        target: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl MockReq {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        }

        fn body_str(&self) -> String {
            String::from_utf8_lossy(&self.body).into_owned()
        }

        fn body_json(&self) -> serde_json::Value {
            serde_json::from_slice(&self.body).expect("request body is JSON")
        }
    }

    /// Canned reply: status code + raw body.
    struct MockReply {
        status: u16,
        body: Vec<u8>,
    }

    fn reply(status: u16, body: impl AsRef<[u8]>) -> MockReply {
        MockReply {
            status,
            body: body.as_ref().to_vec(),
        }
    }

    /// A `POST /sandboxes/{id}/process` reply in the legacy nested shape.
    fn run_reply(exit_code: i32, stdout: &str, stderr: &str) -> MockReply {
        reply(
            200,
            json!({ "run": { "exit_code": exit_code, "stdout": stdout, "stderr": stderr } })
                .to_string(),
        )
    }

    type MockLog = Arc<std::sync::Mutex<Vec<MockReq>>>;

    fn read_request(stream: &mut std::net::TcpStream) -> Option<MockReq> {
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
            let n = stream.read(&mut chunk).ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let mut lines = head.split("\r\n");
        let mut request_line = lines.next()?.split(' ');
        let method = request_line.next()?.to_string();
        let target = request_line.next()?.to_string();
        let headers: Vec<(String, String)> = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        let len: usize = headers
            .iter()
            .find(|(k, _)| k == "content-length")
            .and_then(|(_, v)| v.parse().ok())
            .unwrap_or(0);
        let mut body = buf[head_end..].to_vec();
        while body.len() < len {
            let n = stream.read(&mut chunk).ok()?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
        body.truncate(len);
        Some(MockReq {
            method,
            target,
            headers,
            body,
        })
    }

    /// Loopback stand-in for the E2B REST API. Every connection is parsed
    /// into a [`MockReq`], answered by `route` with `Connection: close`
    /// (so reqwest opens a fresh connection per call), and appended to the
    /// returned log *before* the reply is written — once a client call
    /// returns, its request is visible to the test. Returns the base URL.
    fn spawn_e2b_mock(route: impl Fn(&MockReq) -> MockReply + Send + 'static) -> (String, MockLog) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let log: MockLog = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&log);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let Some(req) = read_request(&mut stream) else {
                    continue;
                };
                let rep = route(&req);
                sink.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(req);
                let head = format!(
                    "HTTP/1.1 {} Mock\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    rep.status,
                    rep.body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&rep.body);
                let _ = stream.flush();
            }
        });
        (format!("http://{addr}"), log)
    }

    fn requests(log: &MockLog) -> std::sync::MutexGuard<'_, Vec<MockReq>> {
        log.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn config_at(api_base: &str, timeout_secs: u32) -> E2bConfig {
        E2bConfig {
            api_key: "k-test".into(),
            template_id: "custom".into(),
            timeout_secs,
            api_base: api_base.to_string(),
        }
    }

    /// A sandbox handle as `create` would have returned it, bound to a mock.
    fn sandbox_at(api_base: &str, id: &str) -> E2bSandbox {
        E2bSandbox {
            config: config_at(api_base, 1234),
            sandbox_id: id.to_string(),
            client: reqwest::Client::new(),
        }
    }

    /// Router for the whole transport lifecycle against sandbox `sb-1`:
    /// create, the `/workspace` mkdir, the two capability probes, an
    /// `exec_shell` round trip, TTL renewal and teardown.
    fn transport_route(req: &MockReq) -> MockReply {
        match (req.method.as_str(), req.target.as_str()) {
            ("POST", "/sandboxes") => reply(200, r#"{"sandboxId":"sb-1"}"#),
            ("POST", "/sandboxes/sb-1/process") => {
                let body = req.body_str();
                if body.contains("whoami") {
                    run_reply(0, "vm-user\n", "")
                } else if body.contains("command -v") {
                    run_reply(0, "git\nrg\n", "")
                } else if body.contains("mkdir -p") {
                    run_reply(0, "", "")
                } else {
                    run_reply(0, "hi\n", "")
                }
            }
            ("PATCH", "/sandboxes/sb-1") | ("DELETE", "/sandboxes/sb-1") => reply(200, "{}"),
            _ => reply(404, "unexpected request"),
        }
    }

    #[test]
    fn sandbox_id_exposes_the_id_the_api_assigned() {
        let (base, _log) = spawn_e2b_mock(|_| reply(200, "{}"));
        let sb = sandbox_at(&base, "sb-id-123");
        assert_eq!(sb.sandbox_id(), "sb-id-123");
    }

    #[tokio::test]
    async fn exec_raw_forwards_the_command_and_returns_the_nested_run_triple() {
        let (base, log) = spawn_e2b_mock(|_| run_reply(7, "out-value", "err-value"));
        let sb = sandbox_at(&base, "sb-raw");
        let got = sb.exec_raw("printf hi", 42).await.expect("exec_raw");
        assert_eq!(got, (7, "out-value".to_string(), "err-value".to_string()));
        let log = requests(&log);
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].method, "POST");
        assert_eq!(log[0].target, "/sandboxes/sb-raw/process");
        assert_eq!(log[0].header("x-api-key"), Some("k-test"));
        assert_eq!(
            log[0].body_json(),
            json!({ "cmds": ["printf hi"], "timeout": 42 })
        );
    }

    #[tokio::test]
    async fn exec_raw_accepts_the_flat_response_shape() {
        let (base, _log) = spawn_e2b_mock(|_| {
            reply(
                200,
                r#"{"exit_code":2,"stdout":"flat-out","stderr":"flat-err"}"#,
            )
        });
        let sb = sandbox_at(&base, "sb-flat");
        let got = sb.exec_raw("true", 5).await.expect("exec_raw");
        assert_eq!(got, (2, "flat-out".to_string(), "flat-err".to_string()));
    }

    #[tokio::test]
    async fn exec_raw_surfaces_http_failures_as_storage_errors() {
        let (base, _log) = spawn_e2b_mock(|_| reply(502, "upstream down"));
        let sb = sandbox_at(&base, "sb-raw-err");
        match sb.exec_raw("true", 5).await {
            Err(Error::Storage { message }) => {
                assert!(message.contains("e2b exec HTTP error"), "{message}");
            }
            other => panic!("expected Error::Storage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn exec_returns_stdout_and_appends_stderr_only_when_present() {
        let (base, _log) = spawn_e2b_mock(|req| {
            if req.body_str().contains("quiet") {
                run_reply(0, "s-out", "")
            } else {
                run_reply(0, "s-out", "s-err")
            }
        });
        let sb = sandbox_at(&base, "sb-exec");
        assert_eq!(sb.exec("quiet", 5).await.expect("exec"), "s-out");
        assert_eq!(
            sb.exec("noisy", 5).await.expect("exec"),
            "s-out\n[stderr]: s-err"
        );
    }

    #[tokio::test]
    async fn exec_turns_a_non_zero_exit_into_a_bash_tool_error() {
        let (base, _log) = spawn_e2b_mock(|_| run_reply(3, "partial", "boom"));
        let sb = sandbox_at(&base, "sb-exec-fail");
        match sb.exec("false", 5).await {
            Err(Error::Tool {
                name,
                call_id,
                message,
            }) => {
                assert_eq!(name, "Bash");
                assert_eq!(call_id, None);
                assert!(message.contains("command exited with code 3"), "{message}");
                assert!(message.contains("boom"), "{message}");
            }
            other => panic!("expected Error::Tool, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upload_file_posts_the_raw_body_under_the_path_query() {
        let (base, log) = spawn_e2b_mock(|_| reply(200, "{}"));
        let sb = sandbox_at(&base, "sb-up");
        sb.upload_file("/workspace/dir/a.txt", b"hello\x00world")
            .await
            .expect("upload");
        let log = requests(&log);
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].method, "POST");
        assert_eq!(
            log[0].target,
            "/sandboxes/sb-up/files?path=%2Fworkspace%2Fdir%2Fa.txt"
        );
        assert_eq!(log[0].header("x-api-key"), Some("k-test"));
        assert_eq!(log[0].body, b"hello\x00world");
    }

    #[tokio::test]
    async fn upload_file_maps_http_failures_to_io_errors() {
        let (base, _log) = spawn_e2b_mock(|_| reply(403, "key revoked"));
        let sb = sandbox_at(&base, "sb-up-err");
        let err = sb
            .upload_file("/workspace/a.txt", b"x")
            .await
            .expect_err("403 must fail the upload");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        let msg = err.to_string();
        assert!(msg.contains("e2b HTTP 403"), "{msg}");
        assert!(msg.contains("key revoked"), "{msg}");
    }

    #[tokio::test]
    async fn download_file_returns_the_body_bytes_verbatim() {
        let payload: &'static [u8] = b"payload\x00\xff\n";
        let (base, log) = spawn_e2b_mock(move |_| reply(200, payload));
        let sb = sandbox_at(&base, "sb-down");
        let got = sb
            .download_file("/workspace/b.bin")
            .await
            .expect("download");
        assert_eq!(got, payload);
        let log = requests(&log);
        assert_eq!(log[0].method, "GET");
        assert_eq!(
            log[0].target,
            "/sandboxes/sb-down/files?path=%2Fworkspace%2Fb.bin"
        );
        assert_eq!(log[0].header("x-api-key"), Some("k-test"));
    }

    #[tokio::test]
    async fn download_file_maps_404_to_not_found() {
        let (base, _log) = spawn_e2b_mock(|_| reply(404, "no such file"));
        let sb = sandbox_at(&base, "sb-down-404");
        let err = sb
            .download_file("/workspace/missing")
            .await
            .expect_err("404 must fail the download");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        let msg = err.to_string();
        assert!(msg.contains("e2b HTTP 404"), "{msg}");
        assert!(msg.contains("no such file"), "{msg}");
    }

    #[tokio::test]
    async fn refresh_timeout_patches_the_configured_ttl() {
        let (base, log) = spawn_e2b_mock(|_| reply(200, "{}"));
        let sb = sandbox_at(&base, "sb-ttl");
        sb.refresh_timeout().await.expect("refresh");
        let log = requests(&log);
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].method, "PATCH");
        assert_eq!(log[0].target, "/sandboxes/sb-ttl");
        assert_eq!(log[0].header("x-api-key"), Some("k-test"));
        assert_eq!(log[0].body_json(), json!({ "timeout": 1234 }));
    }

    #[tokio::test]
    async fn refresh_timeout_surfaces_http_failures() {
        let (base, _log) = spawn_e2b_mock(|_| reply(500, "ttl backend down"));
        let sb = sandbox_at(&base, "sb-ttl-err");
        match sb.refresh_timeout().await {
            Err(Error::Storage { message }) => {
                assert!(
                    message.contains("e2b refresh_timeout HTTP 500"),
                    "{message}"
                );
                assert!(message.contains("ttl backend down"), "{message}");
            }
            other => panic!("expected Error::Storage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn delete_is_authenticated_and_treats_404_as_idempotent_success() {
        let (base, log) = spawn_e2b_mock(|req| {
            if req.target.ends_with("sb-gone") {
                reply(404, "not found")
            } else {
                reply(200, "{}")
            }
        });
        let sb = sandbox_at(&base, "sb-del");
        sb.delete().await.expect("delete 200");
        let gone = sandbox_at(&base, "sb-gone");
        gone.delete()
            .await
            .expect("404 is success for an idempotent delete");
        let log = requests(&log);
        assert_eq!(log.len(), 2);
        assert_eq!(
            (log[0].method.as_str(), log[0].target.as_str()),
            ("DELETE", "/sandboxes/sb-del")
        );
        assert_eq!(log[0].header("x-api-key"), Some("k-test"));
        assert_eq!(
            (log[1].method.as_str(), log[1].target.as_str()),
            ("DELETE", "/sandboxes/sb-gone")
        );
    }

    #[tokio::test]
    async fn delete_surfaces_non_404_failures() {
        let (base, _log) = spawn_e2b_mock(|_| reply(500, "backend down"));
        let sb = sandbox_at(&base, "sb-del-err");
        match sb.delete().await {
            Err(Error::Storage { message }) => {
                assert!(message.contains("e2b delete HTTP 500"), "{message}");
                assert!(message.contains("backend down"), "{message}");
            }
            other => panic!("expected Error::Storage, got {other:?}"),
        }
    }

    #[test]
    fn drop_reclaims_the_sandbox_even_outside_a_runtime() {
        // No ambient runtime here, so `Handle::try_current()` fails and Drop
        // must take the blocking current-thread fallback — the DELETE has
        // completed by the time `drop` returns. The env opt-out is the
        // documented escape hatch and must suppress it.
        let _env = crate::test_util::env_lock();
        let saved = std::env::var("RECURSIVE_E2B_NO_DROP_DELETE").ok();
        std::env::remove_var("RECURSIVE_E2B_NO_DROP_DELETE");
        let (base, log) = spawn_e2b_mock(|_| reply(200, "{}"));
        drop(sandbox_at(&base, "sb-drop"));
        std::env::set_var("RECURSIVE_E2B_NO_DROP_DELETE", "1");
        drop(sandbox_at(&base, "sb-kept"));
        match saved {
            Some(v) => std::env::set_var("RECURSIVE_E2B_NO_DROP_DELETE", v),
            None => std::env::remove_var("RECURSIVE_E2B_NO_DROP_DELETE"),
        }
        let log = requests(&log);
        assert_eq!(
            log.len(),
            1,
            "exactly one DELETE: the opted-out drop sends nothing"
        );
        assert_eq!(log[0].method, "DELETE");
        assert_eq!(log[0].target, "/sandboxes/sb-drop");
        assert_eq!(log[0].header("x-api-key"), Some("k-test"));
    }

    #[tokio::test]
    async fn ensure_started_creates_the_vm_root_and_probes_capabilities_once() {
        let (base, log) = spawn_e2b_mock(transport_route);
        let t = E2bTransport::new(config_at(&base, 3600), "/host/ws");
        t.ensure_started().await.expect("ensure_started");
        t.ensure_started()
            .await
            .expect("second call must be a no-op");
        let caps = t.capabilities();
        assert_eq!(
            caps.user.as_deref(),
            Some("vm-user"),
            "the whoami probe must replace the pre-probe default"
        );
        assert_eq!(caps.toolchain, vec!["git".to_string(), "rg".to_string()]);
        assert_eq!(caps.path_root, PathBuf::from("/workspace"));
        let log = requests(&log);
        let creates = log
            .iter()
            .filter(|r| r.method == "POST" && r.target == "/sandboxes")
            .count();
        assert_eq!(creates, 1, "one sandbox per transport");
        assert_eq!(log[0].header("x-api-key"), Some("k-test"));
        assert_eq!(
            log[0].body_json(),
            json!({ "template_id": "custom", "timeout": 3600 })
        );
        assert_eq!(log[1].target, "/sandboxes/sb-1/process");
        assert_eq!(log[1].body_json()["cmds"], json!(["mkdir -p '/workspace'"]));
        assert_eq!(
            log.len(),
            4,
            "create + mkdir + whoami + toolchain; the no-op second call adds nothing"
        );
    }

    #[tokio::test]
    async fn ensure_started_fails_when_the_vm_root_cannot_be_created() {
        let (base, _log) = spawn_e2b_mock(|req| {
            if req.body_str().contains("mkdir -p") {
                run_reply(1, "", "mkdir: read-only file system")
            } else {
                transport_route(req)
            }
        });
        let t = E2bTransport::new(config_at(&base, 3600), "/host/ws");
        match t.ensure_started().await {
            Err(Error::Storage { message }) => {
                assert!(
                    message.contains("e2b mkdir workspace root failed"),
                    "{message}"
                );
                assert!(message.contains("read-only file system"), "{message}");
            }
            other => panic!("expected Error::Storage, got {other:?}"),
        }
        assert!(
            t.sandbox.lock().await.is_none(),
            "a sandbox whose root could not be created is not kept"
        );
        assert_eq!(
            t.capabilities().user.as_deref(),
            Some("root"),
            "no probe ran past the failed mkdir"
        );
    }

    #[tokio::test]
    async fn ttl_is_renewed_once_until_half_the_timeout_has_elapsed() {
        let (base, log) = spawn_e2b_mock(transport_route);
        let t = E2bTransport::new(config_at(&base, 3600), "/host/ws");
        for _ in 0..2 {
            let r = t
                .exec_shell(
                    "echo hi",
                    Path::new("/host/ws"),
                    &[],
                    Duration::from_secs(30),
                    4096,
                )
                .await
                .expect("exec_shell");
            assert_eq!(r.exit_code, Some(0));
            assert_eq!(r.stdout, "hi\n");
        }
        let log = requests(&log);
        let patches: Vec<&MockReq> = log.iter().filter(|r| r.method == "PATCH").collect();
        assert_eq!(
            patches.len(),
            1,
            "the first exec renews (never renewed yet); the second, milliseconds \
             later, is far short of timeout/2"
        );
        assert_eq!(patches[0].target, "/sandboxes/sb-1");
        assert_eq!(patches[0].body_json(), json!({ "timeout": 3600 }));
        let script = log
            .iter()
            .find(|r| r.body_str().contains("echo hi"))
            .expect("the exec_shell command reached the VM");
        assert_eq!(
            script.body_json()["cmds"],
            json!(["cd '/workspace' && sh -c 'echo hi'"])
        );
    }

    #[tokio::test]
    async fn ttl_renewal_is_due_once_half_the_timeout_has_elapsed() {
        // timeout 20 s → renewal threshold 10 s. The renewal clock is
        // backdated rather than slept on.
        let (base, log) = spawn_e2b_mock(transport_route);
        let t = E2bTransport::new(config_at(&base, 20), "/host/ws");
        t.ensure_started().await.expect("ensure_started");
        let patches = || {
            requests(&log)
                .iter()
                .filter(|r| r.method == "PATCH")
                .count()
        };
        let ago = |secs| {
            std::time::Instant::now()
                .checked_sub(Duration::from_secs(secs))
                .expect("the monotonic clock reaches back that far")
        };

        *t.last_refresh.lock().await = Some(ago(15));
        t.maybe_refresh_ttl().await.expect("renewal");
        assert_eq!(
            patches(),
            1,
            "15 s since the last renewal is past timeout/2 = 10 s"
        );

        *t.last_refresh.lock().await = Some(ago(2));
        t.maybe_refresh_ttl().await.expect("no renewal due");
        assert_eq!(
            patches(),
            1,
            "2 s since the last renewal is short of timeout/2 = 10 s"
        );

        let log = requests(&log);
        let patch = log
            .iter()
            .find(|r| r.method == "PATCH")
            .expect("the renewal");
        assert_eq!(patch.target, "/sandboxes/sb-1");
        assert_eq!(patch.body_json(), json!({ "timeout": 20 }));
    }

    #[tokio::test]
    async fn a_failed_ttl_renewal_fails_the_exec_and_is_retried() {
        let (base, log) = spawn_e2b_mock(|req| {
            if req.method == "PATCH" {
                reply(500, "ttl backend down")
            } else {
                transport_route(req)
            }
        });
        let t = E2bTransport::new(config_at(&base, 3600), "/host/ws");
        for attempt in 1..=2 {
            let err = t
                .exec_shell(
                    "echo hi",
                    Path::new("/host/ws"),
                    &[],
                    Duration::from_secs(30),
                    4096,
                )
                .await
                .expect_err("a failed renewal must surface, never be swallowed");
            let msg = err.to_string();
            assert!(
                msg.contains("e2b refresh_timeout HTTP 500"),
                "attempt {attempt}: {msg}"
            );
            assert!(msg.contains("ttl backend down"), "attempt {attempt}: {msg}");
        }
        let patches = requests(&log)
            .iter()
            .filter(|r| r.method == "PATCH")
            .count();
        assert_eq!(
            patches, 2,
            "a failed renewal is not recorded as one — the next exec retries it"
        );
    }

    #[tokio::test]
    async fn vm_exec_starts_the_sandbox_on_demand_and_returns_the_raw_triple() {
        let (base, log) = spawn_e2b_mock(|req| {
            if req.body_str().contains("vm-cmd") {
                run_reply(42, "vm-out", "vm-err")
            } else {
                transport_route(req)
            }
        });
        let t = E2bTransport::new(config_at(&base, 3600), "/host/ws");
        let got = t
            .vm_exec("vm-cmd", Duration::from_secs(7))
            .await
            .expect("vm_exec");
        assert_eq!(
            got,
            (42, "vm-out".to_string(), "vm-err".to_string()),
            "a non-zero exit and stderr are data, returned verbatim"
        );
        t.vm_exec("vm-cmd", Duration::from_millis(250))
            .await
            .expect("vm_exec");
        let log = requests(&log);
        let creates = log
            .iter()
            .filter(|r| r.method == "POST" && r.target == "/sandboxes")
            .count();
        assert_eq!(creates, 1, "created on first use, reused afterwards");
        let runs: Vec<serde_json::Value> = log
            .iter()
            .filter(|r| r.body_str().contains("vm-cmd"))
            .map(MockReq::body_json)
            .collect();
        assert_eq!(
            runs,
            vec![
                json!({ "cmds": ["vm-cmd"], "timeout": 7 }),
                json!({ "cmds": ["vm-cmd"], "timeout": 1 }),
            ],
            "the timeout is sent in whole seconds, never 0"
        );
    }

    #[tokio::test]
    async fn vm_exec_io_passes_the_triple_through_and_maps_failures_to_io_errors() {
        let (base, _log) = spawn_e2b_mock(|req| {
            if req.body_str().contains("io-cmd") {
                run_reply(9, "io-out", "io-err")
            } else {
                transport_route(req)
            }
        });
        let t = E2bTransport::new(config_at(&base, 3600), "/host/ws");
        assert_eq!(
            t.vm_exec_io("io-cmd", Duration::from_secs(5))
                .await
                .expect("vm_exec_io"),
            (9, "io-out".to_string(), "io-err".to_string())
        );

        let (down, _down_log) = spawn_e2b_mock(|_| reply(503, "e2b unavailable"));
        let t_down = E2bTransport::new(config_at(&down, 3600), "/host/ws");
        let err = t_down
            .vm_exec_io("io-cmd", Duration::from_secs(5))
            .await
            .expect_err("a sandbox that cannot be created must surface");
        assert_eq!(err.kind(), std::io::ErrorKind::Other);
        let msg = err.to_string();
        assert!(msg.contains("e2b create sandbox HTTP error"), "{msg}");
        assert!(msg.contains("503"), "{msg}");
    }

    #[tokio::test]
    async fn probe_trim_returns_the_trimmed_stdout_or_none() {
        let (base, log) = spawn_e2b_mock(|req| {
            let body = req.body_str();
            if body.contains("padded") {
                run_reply(0, "  vm-user \n", "")
            } else if body.contains("blank") {
                run_reply(0, " \n\t", "")
            } else if body.contains("failing") {
                run_reply(1, "vm-user\n", "denied")
            } else {
                reply(500, "probe backend down")
            }
        });
        let sb = sandbox_at(&base, "sb-probe");
        assert_eq!(probe_trim(&sb, "padded").await.as_deref(), Some("vm-user"));
        assert_eq!(
            probe_trim(&sb, "blank").await,
            None,
            "blank stdout is no answer"
        );
        assert_eq!(
            probe_trim(&sb, "failing").await,
            None,
            "a non-zero exit is no answer, whatever its stdout"
        );
        assert_eq!(
            probe_trim(&sb, "unrouted").await,
            None,
            "a failed request is no answer"
        );
        let log = requests(&log);
        assert_eq!(log.len(), 4);
        assert_eq!(log[0].target, "/sandboxes/sb-probe/process");
        assert_eq!(
            log[0].body_json(),
            json!({ "cmds": ["padded"], "timeout": 10 })
        );
    }

    #[test]
    fn cap_string_respects_exact_length_and_char_boundaries() {
        // len == max must stay verbatim — no truncation marker appears.
        let mut exact = "x".repeat(10);
        cap_string(&mut exact, 10);
        assert_eq!(
            exact,
            "x".repeat(10),
            "a string of exactly max bytes is kept"
        );

        // A max landing inside a multi-byte char keeps the longest
        // char-boundary-safe prefix ("héy": byte 2 is inside 'é').
        let mut s = String::from("héy");
        cap_string(&mut s, 2);
        assert_eq!(s, "h\n... [output truncated]");
    }

    #[tokio::test]
    async fn destroy_tears_down_the_sandbox_exactly_once() {
        let (base, log) = spawn_e2b_mock(transport_route);
        let t = E2bTransport::new(config_at(&base, 3600), "/host/ws");
        t.ensure_started().await.expect("ensure_started");
        t.destroy().await;
        t.destroy().await;
        assert!(
            t.sandbox.lock().await.is_none(),
            "destroy takes the sandbox — a destroyed transport holds none"
        );
        let log = requests(&log);
        let deletes: Vec<&MockReq> = log.iter().filter(|r| r.method == "DELETE").collect();
        assert_eq!(deletes.len(), 1, "destroy is idempotent: one DELETE total");
        assert_eq!(deletes[0].target, "/sandboxes/sb-1");
        assert_eq!(deletes[0].header("x-api-key"), Some("k-test"));
    }

    #[tokio::test]
    async fn read_file_downloads_the_file_bytes_verbatim() {
        let payload: &'static [u8] = b"vm file\x00\xff";
        let (base, log) = spawn_e2b_mock(move |req| {
            if req.method == "GET" && req.target.starts_with("/sandboxes/sb-1/files") {
                reply(200, payload)
            } else {
                transport_route(req)
            }
        });
        let t = E2bTransport::new(config_at(&base, 3600), "/host/ws");
        let got = t
            .read_file(Path::new("/host/ws/a.bin"))
            .await
            .expect("read_file");
        assert_eq!(got, payload, "file bytes cross the VM boundary verbatim");
        let log = requests(&log);
        let get = log
            .iter()
            .find(|r| r.method == "GET")
            .expect("the download reaches the VM");
        assert_eq!(
            get.target,
            "/sandboxes/sb-1/files?path=%2Fworkspace%2Fa.bin"
        );
    }

    #[tokio::test]
    async fn write_file_creates_the_parent_dir_then_uploads_the_bytes() {
        let (base, log) = spawn_e2b_mock(|req| {
            if req.method == "POST" && req.target.starts_with("/sandboxes/sb-1/files") {
                reply(200, "{}")
            } else {
                transport_route(req)
            }
        });
        let t = E2bTransport::new(config_at(&base, 3600), "/host/ws");
        t.write_file(Path::new("/host/ws/dir/a.txt"), b"hello")
            .await
            .expect("write_file");
        {
            let log = requests(&log);
            let mkdir = log
                .iter()
                .find(|r| r.body_str().contains("mkdir -p '/workspace/dir'"))
                .expect("the parent directory is created inside the VM");
            assert_eq!(mkdir.body_json()["cmds"][0], "mkdir -p '/workspace/dir'");
            let upload = log
                .iter()
                .find(|r| r.method == "POST" && r.target.starts_with("/sandboxes/sb-1/files"))
                .expect("the file is uploaded");
            assert_eq!(
                upload.target,
                "/sandboxes/sb-1/files?path=%2Fworkspace%2Fdir%2Fa.txt"
            );
            assert_eq!(upload.body, b"hello");
        }

        // A failed parent mkdir fails the whole write; nothing is uploaded.
        let (bad, bad_log) = spawn_e2b_mock(|req| {
            if req.body_str().contains("mkdir -p '/workspace/dir'") {
                run_reply(7, "", "mkdir: cannot create directory")
            } else {
                transport_route(req)
            }
        });
        let t_bad = E2bTransport::new(config_at(&bad, 3600), "/host/ws");
        let err = t_bad
            .write_file(Path::new("/host/ws/dir/a.txt"), b"hello")
            .await
            .expect_err("a failed parent mkdir must fail the write");
        let msg = err.to_string();
        assert!(msg.contains("mkdir failed in e2b vm"), "{msg}");
        assert!(msg.contains("cannot create directory"), "{msg}");
        assert!(
            !requests(&bad_log)
                .iter()
                .any(|r| r.target.contains("/files")),
            "no upload is attempted past the failed mkdir"
        );
    }

    #[tokio::test]
    async fn list_dir_parses_the_find_output_and_maps_failures() {
        let (base, log) = spawn_e2b_mock(|req| {
            if req.body_str().contains("%f") {
                run_reply(0, "d\tsub\nf\ta.txt\n", "")
            } else {
                transport_route(req)
            }
        });
        let t = E2bTransport::new(config_at(&base, 3600), "/host/ws");
        let entries = t
            .list_dir(Path::new("/host/ws/sub"))
            .await
            .expect("list_dir");
        assert_eq!(
            entries
                .iter()
                .map(|e| (e.name.as_str(), e.is_dir))
                .collect::<Vec<_>>(),
            vec![("sub", true), ("a.txt", false)]
        );
        {
            let log = requests(&log);
            let find = log
                .iter()
                .find(|r| r.body_str().contains("%f"))
                .expect("the find command reaches the VM");
            assert_eq!(
                find.body_json()["cmds"][0],
                "find '/workspace/sub' -mindepth 1 -maxdepth 1 -printf '%y\\t%f\\n'"
            );
        }

        // A non-zero find is NotFound, whatever its stderr says.
        let (bad, _bad_log) = spawn_e2b_mock(|req| {
            if req.body_str().contains("%f") {
                run_reply(1, "", "find: raced away")
            } else {
                transport_route(req)
            }
        });
        let t_bad = E2bTransport::new(config_at(&bad, 3600), "/host/ws");
        let err = t_bad
            .list_dir(Path::new("/host/ws/sub"))
            .await
            .expect_err("a non-zero find must fail the listing");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        let msg = err.to_string();
        assert!(msg.contains("dir not found in e2b vm"), "{msg}");
        assert!(msg.contains("raced away"), "{msg}");

        // So is a zero exit with "No such file or directory" on stderr.
        let (gone, _gone_log) = spawn_e2b_mock(|req| {
            if req.body_str().contains("%f") {
                run_reply(0, "", "find: '/workspace/sub': No such file or directory")
            } else {
                transport_route(req)
            }
        });
        let t_gone = E2bTransport::new(config_at(&gone, 3600), "/host/ws");
        let err = t_gone
            .list_dir(Path::new("/host/ws/sub"))
            .await
            .expect_err("a missing directory must be NotFound");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn walk_runs_one_find_and_enforces_depth_prune_and_ignore() {
        // The VM's find output, deliberately including an entry under an
        // ignored dir (the client-side filter is defence in depth).
        let walk_stdout = "f\t/workspace/src/a.rs\t1\nd\t/workspace/src/sub\t2\nf\t/workspace/target/x.rs\t3\nf\t/workspace/README.md\t4\n";
        let (base, log) = spawn_e2b_mock(move |req| {
            if req.body_str().contains("%p") {
                run_reply(0, walk_stdout, "")
            } else {
                transport_route(req)
            }
        });
        let t = E2bTransport::new(config_at(&base, 3600), "/host/ws");
        let opts = WalkOptions {
            max_depth: 5,
            ignore_dirs: vec!["target".into()],
            ..Default::default()
        };
        let entries = t.walk(Path::new("/host/ws"), &opts).await.expect("walk");
        assert_eq!(
            entries,
            vec![
                WalkEntry {
                    path: PathBuf::from("README.md"),
                    is_file: true,
                    size: 4,
                },
                WalkEntry {
                    path: PathBuf::from("src/a.rs"),
                    is_file: true,
                    size: 1,
                },
                WalkEntry {
                    path: PathBuf::from("src/sub"),
                    is_file: false,
                    size: 2,
                },
            ],
            "root-relative, sorted, ignore_dirs pruned"
        );
        {
            let log = requests(&log);
            let finds: Vec<&MockReq> = log.iter().filter(|r| r.body_str().contains("%p")).collect();
            assert_eq!(finds.len(), 1, "a walk is a single round trip");
            assert_eq!(
                finds[0].body_json()["cmds"][0],
                "find '/workspace' -maxdepth 5 \\( -name 'target' \\) -prune -o -printf '%y\\t%p\\t%s\\n'"
            );
        }

        // A non-zero find is NotFound.
        let (bad, _bad_log) = spawn_e2b_mock(|req| {
            if req.body_str().contains("%p") {
                run_reply(1, "", "find: bad root")
            } else {
                transport_route(req)
            }
        });
        let t_bad = E2bTransport::new(config_at(&bad, 3600), "/host/ws");
        let err = t_bad
            .walk(Path::new("/host/ws"), &opts)
            .await
            .expect_err("a non-zero find must fail the walk");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        let msg = err.to_string();
        assert!(msg.contains("walk root not found in e2b vm"), "{msg}");
    }

    #[tokio::test]
    async fn create_dir_all_maps_the_path_and_surfaces_failures() {
        let (base, log) = spawn_e2b_mock(transport_route);
        let t = E2bTransport::new(config_at(&base, 3600), "/host/ws");
        t.create_dir_all(Path::new("/host/ws/sub/deep"))
            .await
            .expect("create_dir_all");
        {
            let log = requests(&log);
            let mkdir = log
                .iter()
                .find(|r| r.body_str().contains("mkdir -p '/workspace/sub/deep'"))
                .expect("the mkdir command reaches the VM");
            assert_eq!(
                mkdir.body_json()["cmds"][0],
                "mkdir -p '/workspace/sub/deep'"
            );
        }

        // A non-zero mkdir surfaces the VM's stderr.
        let (bad, _bad_log) = spawn_e2b_mock(|req| {
            if req.body_str().contains("mkdir -p '/workspace/sub/deep'") {
                run_reply(3, "", "mkdir: read-only file system")
            } else {
                transport_route(req)
            }
        });
        let t_bad = E2bTransport::new(config_at(&bad, 3600), "/host/ws");
        let err = t_bad
            .create_dir_all(Path::new("/host/ws/sub/deep"))
            .await
            .expect_err("a failed mkdir must be an error");
        let msg = err.to_string();
        assert!(msg.contains("mkdir failed in e2b vm"), "{msg}");
        assert!(msg.contains("read-only file system"), "{msg}");
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
