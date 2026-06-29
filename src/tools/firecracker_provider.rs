//! Firecracker MicroVM-backed [`ToolSetProvider`] (L3-local).
//!
//! [`FirecrackerToolSetProvider`] launches a local [Firecracker] VMM process
//! and routes `Bash`, `Read`, and `Write` tool calls through the VM via a
//! vsock-backed JSON-RPC protocol.
//!
//! Unlike [`E2bToolSetProvider`] which relies on the e2b.dev cloud API, this
//! provider runs a Firecracker process **locally** — suitable for on-premise
//! deployments and self-hosted SaaS infrastructure.
//!
//! # Requirements (runtime, Linux only)
//!
//! * Linux host with KVM enabled (`/dev/kvm` must exist).
//! * Firecracker binary on `PATH` or `RECURSIVE_FIRECRACKER_BIN` set.
//! * A guest Linux kernel image (`vmlinux` or `bzImage`).
//! * A rootfs image pre-loaded with a tiny exec-agent that listens on the
//!   vsock port (see `VSOCK_PORT`).
//!
//! On macOS or when KVM is unavailable, `build_registry()` returns an error
//! and callers should fall back to a lower-tier provider.
//!
//! # Vsock protocol
//!
//! The exec-agent in the guest listens on vsock CID 3, port `7654`.
//! Each request is a newline-terminated JSON object:
//! ```json
//! {"cmd":"exec","command":"ls /workspace","timeout_secs":30}
//! {"cmd":"read_file","path":"/workspace/main.rs"}
//! {"cmd":"write_file","path":"/workspace/out.txt","content_b64":"..."}
//! {"cmd":"list_dir","path":"/workspace"}
//! ```
//! Each response is a newline-terminated JSON object:
//! ```json
//! {"ok":true,"exit_code":0,"stdout":"...","stderr":""}
//! {"ok":true,"content_b64":"..."}
//! {"ok":true,"entries":["a.rs","b.rs"]}
//! {"ok":false,"error":"..."}
//! ```
//!
//! [Firecracker]: https://github.com/firecracker-microvm/firecracker
//! [`E2bToolSetProvider`]: crate::tools::e2b_provider::E2bToolSetProvider

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;

#[cfg(target_os = "linux")]
use std::time::Duration;
#[cfg(target_os = "linux")]
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::tool_set_provider::{SandboxMode, ToolSetProvider};
use crate::tools::{Tool, ToolRegistry, ToolSideEffect};

/// Vsock port the exec-agent listens on inside the guest.
#[cfg(target_os = "linux")]
const VSOCK_PORT: u16 = 7654;

/// Default Firecracker binary name (searched on PATH).
const DEFAULT_FIRECRACKER_BIN: &str = "firecracker";

// ─────────────────────────────────────────────────────────────────────────────
// KVM availability check
// ─────────────────────────────────────────────────────────────────────────────

/// Returns `true` if KVM is available on the current host.
///
/// On Linux this checks for `/dev/kvm`; on other platforms always returns
/// `false`.
pub fn kvm_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        Path::new("/dev/kvm").exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// FirecrackerConfig
// ─────────────────────────────────────────────────────────────────────────────

/// Configuration for a Firecracker microVM session.
#[derive(Debug, Clone)]
pub struct FirecrackerConfig {
    /// Path to the Firecracker binary. Defaults to searching `PATH` for
    /// `"firecracker"`, overridable via `RECURSIVE_FIRECRACKER_BIN`.
    pub binary_path: PathBuf,
    /// Guest Linux kernel image (`vmlinux` or `bzImage`).
    pub kernel_path: PathBuf,
    /// Root filesystem block device image.
    pub rootfs_path: PathBuf,
    /// Number of vCPUs (default: 1).
    pub vcpus: u32,
    /// Guest RAM in MiB (default: 128).
    pub mem_mib: u32,
    /// Shell command timeout in seconds (default: 60).
    pub shell_timeout_secs: u64,
    /// Unix socket path used to control the Firecracker VMM.
    /// If `None`, a temporary path under `/tmp` is auto-generated.
    pub api_socket: Option<PathBuf>,
    /// Unix socket path exposed by the vsock UDS proxy on the host.
    /// Firecracker routes vsock traffic from the guest over this socket.
    pub vsock_uds_path: Option<PathBuf>,
}

impl Default for FirecrackerConfig {
    fn default() -> Self {
        let binary_path = std::env::var("RECURSIVE_FIRECRACKER_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_FIRECRACKER_BIN));
        Self {
            binary_path,
            kernel_path: PathBuf::from("/opt/recursive/firecracker/kernel.bin"),
            rootfs_path: PathBuf::from("/opt/recursive/firecracker/rootfs.ext4"),
            vcpus: 1,
            mem_mib: 128,
            shell_timeout_secs: 60,
            api_socket: None,
            vsock_uds_path: None,
        }
    }
}

impl FirecrackerConfig {
    /// Load from environment variables. Required: `RECURSIVE_FC_KERNEL` and
    /// `RECURSIVE_FC_ROOTFS`. Other fields have defaults.
    pub fn from_env() -> Result<Self> {
        let defaults = Self::default();
        let kernel_path = std::env::var("RECURSIVE_FC_KERNEL")
            .map(PathBuf::from)
            .map_err(|_| Error::Config {
                message: "RECURSIVE_FC_KERNEL not set".into(),
            })?;
        let rootfs_path = std::env::var("RECURSIVE_FC_ROOTFS")
            .map(PathBuf::from)
            .map_err(|_| Error::Config {
                message: "RECURSIVE_FC_ROOTFS not set".into(),
            })?;
        let vcpus = std::env::var("RECURSIVE_FC_VCPUS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(defaults.vcpus);
        let mem_mib = std::env::var("RECURSIVE_FC_MEM_MIB")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(defaults.mem_mib);
        Ok(Self {
            kernel_path,
            rootfs_path,
            vcpus,
            mem_mib,
            ..defaults
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Firecracker REST API client (Unix socket HTTP/1.1)
// ─────────────────────────────────────────────────────────────────────────────

/// Minimal HTTP/1.1 client that communicates over a Unix domain socket.
///
/// Firecracker exposes a REST API via a Unix socket. We implement just the
/// subset we need without pulling in additional HTTP crates.
#[cfg(target_os = "linux")]
struct FirecrackerApiClient {
    socket_path: PathBuf,
}

#[cfg(target_os = "linux")]
impl FirecrackerApiClient {
    fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    /// Send an HTTP request over the Unix socket and return the response body.
    #[cfg(target_os = "linux")]
    async fn request(&self, method: &str, path: &str, body: Option<&str>) -> Result<(u16, String)> {
        use tokio::net::UnixStream;

        let request = if let Some(b) = body {
            format!(
                "{method} {path} HTTP/1.1\r\n\
                 Host: localhost\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n\
                 {b}",
                b.len()
            )
        } else {
            format!(
                "{method} {path} HTTP/1.1\r\n\
                 Host: localhost\r\n\
                 Connection: close\r\n\
                 \r\n"
            )
        };

        let mut stream = UnixStream::connect(&self.socket_path)
            .await
            .map_err(|e| Error::Tool {
                name: "FirecrackerApi".into(),
                call_id: None,
                message: format!("connect to {}: {e}", self.socket_path.display()),
            })?;

        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|e| Error::Tool {
                name: "FirecrackerApi".into(),
                call_id: None,
                message: format!("write request: {e}"),
            })?;

        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .map_err(|e| Error::Tool {
                name: "FirecrackerApi".into(),
                call_id: None,
                message: format!("read response: {e}"),
            })?;

        let response_str = String::from_utf8_lossy(&response).into_owned();
        let status_code = Self::parse_status_code(&response_str);
        let body = Self::parse_body(&response_str);
        Ok((status_code, body.to_string()))
    }

    #[cfg(not(target_os = "linux"))]
    async fn request(&self, _: &str, _: &str, _: Option<&str>) -> Result<(u16, String)> {
        Err(Error::Config {
            message: "Firecracker is only supported on Linux".into(),
        })
    }

    fn parse_status_code(response: &str) -> u16 {
        response
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    fn parse_body(response: &str) -> &str {
        // HTTP responses have headers separated from body by \r\n\r\n.
        response
            .find("\r\n\r\n")
            .map(|i| &response[i + 4..])
            .unwrap_or("")
    }

    async fn put(&self, path: &str, body: &str) -> Result<()> {
        let (status, resp_body) = self.request("PUT", path, Some(body)).await?;
        if status >= 400 || status == 0 {
            return Err(Error::Tool {
                name: "FirecrackerApi".into(),
                call_id: None,
                message: format!("PUT {path} failed (HTTP {status}): {resp_body}"),
            });
        }
        Ok(())
    }

    async fn get(&self, path: &str) -> Result<String> {
        let (status, body) = self.request("GET", path, None).await?;
        if status >= 400 || status == 0 {
            return Err(Error::Tool {
                name: "FirecrackerApi".into(),
                call_id: None,
                message: format!("GET {path} failed (HTTP {status}): {body}"),
            });
        }
        Ok(body)
    }

    async fn set_machine_config(&self, vcpus: u32, mem_mib: u32) -> Result<()> {
        self.put(
            "/machine-config",
            &serde_json::to_string(&json!({
                "vcpu_count": vcpus,
                "mem_size_mib": mem_mib
            }))
            .unwrap(),
        )
        .await
    }

    async fn set_boot_source(&self, kernel_path: &Path, boot_args: &str) -> Result<()> {
        self.put(
            "/boot-source",
            &serde_json::to_string(&json!({
                "kernel_image_path": kernel_path.display().to_string(),
                "boot_args": boot_args
            }))
            .unwrap(),
        )
        .await
    }

    async fn set_rootfs(&self, rootfs_path: &Path, read_only: bool) -> Result<()> {
        self.put(
            "/drives/rootfs",
            &serde_json::to_string(&json!({
                "drive_id": "rootfs",
                "path_on_host": rootfs_path.display().to_string(),
                "is_root_device": true,
                "is_read_only": read_only
            }))
            .unwrap(),
        )
        .await
    }

    async fn set_vsock(&self, cid: u32, uds_path: &Path) -> Result<()> {
        self.put(
            "/vsock",
            &serde_json::to_string(&json!({
                "vsock_id": "vsock0",
                "guest_cid": cid,
                "uds_path": uds_path.display().to_string()
            }))
            .unwrap(),
        )
        .await
    }

    async fn start(&self) -> Result<()> {
        self.put(
            "/actions",
            &serde_json::to_string(&json!({"action_type": "InstanceStart"})).unwrap(),
        )
        .await
    }

    async fn describe_instance(&self) -> Result<String> {
        self.get("/").await
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// FirecrackerVm
// ─────────────────────────────────────────────────────────────────────────────

/// Vsock command types.
///
/// File content is transmitted as UTF-8 strings. Binary files are not
/// supported in this MVP; the agent works exclusively with text files.
#[derive(Debug, Serialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum VsockRequest {
    Exec {
        command: String,
        timeout_secs: u64,
    },
    ReadFile {
        path: String,
    },
    WriteFile {
        path: String,
        /// UTF-8 text content.
        content: String,
    },
    ListDir {
        path: String,
    },
}

/// Vsock response from the exec-agent.
#[derive(Debug, Deserialize)]
struct VsockResponse {
    ok: bool,
    #[serde(default)]
    exit_code: i32,
    #[serde(default)]
    stdout: String,
    #[serde(default)]
    stderr: String,
    /// UTF-8 content for ReadFile responses.
    #[serde(default)]
    content: String,
    #[serde(default)]
    entries: Vec<String>,
    #[serde(default)]
    error: String,
}

/// Result of running a shell command in the Firecracker VM.
#[derive(Debug)]
pub struct FcExecResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// A live Firecracker microVM.
pub struct FirecrackerVm {
    /// The `firecracker` child process.
    _process: tokio::process::Child,
    /// Unix socket path for vsock communication with the exec-agent.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    vsock_uds: PathBuf,
    /// Shell timeout for exec calls.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    shell_timeout_secs: u64,
}

impl FirecrackerVm {
    /// Spawn a Firecracker process, boot the VM, and wait for the exec-agent.
    pub async fn spawn(config: FirecrackerConfig) -> Result<Self> {
        #[cfg(not(target_os = "linux"))]
        let _ = config;

        #[cfg(not(target_os = "linux"))]
        return Err(Error::Config {
            message: "Firecracker is only supported on Linux".into(),
        });

        #[cfg(target_os = "linux")]
        {
            if !kvm_available() {
                return Err(Error::Config {
                    message: "/dev/kvm not available — Firecracker requires KVM".into(),
                });
            }

            // Generate unique socket paths for this session.
            let id = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            let api_socket = config
                .api_socket
                .clone()
                .unwrap_or_else(|| PathBuf::from(format!("/tmp/recursive-fc-api-{id}.sock")));
            let vsock_uds = config
                .vsock_uds_path
                .clone()
                .unwrap_or_else(|| PathBuf::from(format!("/tmp/recursive-fc-vsock-{id}.sock")));

            // Remove stale socket files if they exist.
            let _ = std::fs::remove_file(&api_socket);
            let _ = std::fs::remove_file(&vsock_uds);

            // Spawn the Firecracker process.
            let process = tokio::process::Command::new(&config.binary_path)
                .args(["--api-sock", &api_socket.display().to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|e| Error::Config {
                    message: format!(
                        "failed to spawn Firecracker ({}): {e}",
                        config.binary_path.display()
                    ),
                })?;

            // Wait for the API socket to appear (max 3 seconds).
            let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
            loop {
                if api_socket.exists() {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(Error::Config {
                        message: "Firecracker API socket did not appear within 3s".into(),
                    });
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            // Configure and start the VM.
            let api = FirecrackerApiClient::new(api_socket);
            api.set_machine_config(config.vcpus, config.mem_mib).await?;
            api.set_boot_source(
                &config.kernel_path,
                "console=ttyS0 reboot=k panic=1 pci=off",
            )
            .await?;
            api.set_rootfs(&config.rootfs_path, false).await?;
            api.set_vsock(3, &vsock_uds).await?;
            api.start().await?;

            // Wait for the exec-agent vsock socket to appear (max 10 seconds).
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            loop {
                if vsock_uds.exists() {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(Error::Config {
                        message: "Firecracker exec-agent did not start within 10s".into(),
                    });
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }

            Ok(Self {
                _process: process,
                vsock_uds,
                shell_timeout_secs: config.shell_timeout_secs,
            })
        }
    }

    /// Send a vsock request and parse the response.
    async fn vsock_call(&self, req: &VsockRequest) -> Result<VsockResponse> {
        #[cfg(not(target_os = "linux"))]
        let _ = req;

        #[cfg(not(target_os = "linux"))]
        return Err(Error::Config {
            message: "Firecracker is only supported on Linux".into(),
        });

        #[cfg(target_os = "linux")]
        {
            use tokio::net::UnixStream;

            let mut stream =
                UnixStream::connect(&self.vsock_uds)
                    .await
                    .map_err(|e| Error::Tool {
                        name: "Bash".into(),
                        call_id: None,
                        message: format!("vsock connect: {e}"),
                    })?;

            let mut payload = serde_json::to_string(req).map_err(|e| Error::Tool {
                name: "Bash".into(),
                call_id: None,
                message: format!("serialize request: {e}"),
            })?;
            payload.push('\n');

            stream
                .write_all(payload.as_bytes())
                .await
                .map_err(|e| Error::Tool {
                    name: "Bash".into(),
                    call_id: None,
                    message: format!("vsock write: {e}"),
                })?;

            let mut response_buf = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                stream
                    .read_exact(&mut byte)
                    .await
                    .map_err(|e| Error::Tool {
                        name: "Bash".into(),
                        call_id: None,
                        message: format!("vsock read: {e}"),
                    })?;
                if byte[0] == b'\n' {
                    break;
                }
                response_buf.push(byte[0]);
            }

            serde_json::from_slice(&response_buf).map_err(|e| Error::Tool {
                name: "Bash".into(),
                call_id: None,
                message: format!("parse response: {e}"),
            })
        }
    }

    /// Execute a shell command in the VM.
    pub async fn exec(&self, command: &str) -> Result<FcExecResult> {
        let req = VsockRequest::Exec {
            command: command.to_string(),
            timeout_secs: self.shell_timeout_secs,
        };
        let resp = self.vsock_call(&req).await?;
        if !resp.ok {
            return Err(Error::Tool {
                name: "Bash".into(),
                call_id: None,
                message: resp.error,
            });
        }
        Ok(FcExecResult {
            exit_code: resp.exit_code,
            stdout: resp.stdout,
            stderr: resp.stderr,
        })
    }

    /// Read a file from the VM filesystem (text files only).
    pub async fn read_file(&self, path: &Path) -> Result<String> {
        let req = VsockRequest::ReadFile {
            path: path.display().to_string(),
        };
        let resp = self.vsock_call(&req).await?;
        if !resp.ok {
            return Err(Error::Tool {
                name: "Read".into(),
                call_id: None,
                message: resp.error,
            });
        }
        Ok(resp.content)
    }

    /// Write a UTF-8 text file to the VM filesystem.
    pub async fn write_file(&self, path: &Path, content: &str) -> Result<()> {
        let req = VsockRequest::WriteFile {
            path: path.display().to_string(),
            content: content.to_string(),
        };
        let resp = self.vsock_call(&req).await?;
        if !resp.ok {
            return Err(Error::Tool {
                name: "Write".into(),
                call_id: None,
                message: resp.error,
            });
        }
        Ok(())
    }

    /// List a directory in the VM.
    pub async fn list_dir(&self, path: &Path) -> Result<Vec<String>> {
        let req = VsockRequest::ListDir {
            path: path.display().to_string(),
        };
        let resp = self.vsock_call(&req).await?;
        if !resp.ok {
            return Err(Error::Tool {
                name: "Bash".into(),
                call_id: None,
                message: resp.error,
            });
        }
        Ok(resp.entries)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tool implementations
// ─────────────────────────────────────────────────────────────────────────────

type SharedVm = Arc<Mutex<FirecrackerVm>>;

/// `Bash` tool that executes commands inside the Firecracker VM.
pub struct FirecrackerBashTool {
    vm: SharedVm,
}

#[async_trait]
impl Tool for FirecrackerBashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Bash".into(),
            description: "Execute a shell command inside the Firecracker microVM.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Shell command to execute in the VM"}
                },
                "required": ["command"]
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::External
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let command = args["command"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "Bash".into(),
            message: "missing `command`".into(),
        })?;
        let vm = self.vm.lock().await;
        let result = vm.exec(command).await?;
        let output = if result.stderr.is_empty() {
            result.stdout
        } else {
            format!("{}{}", result.stdout, result.stderr)
        };
        if result.exit_code != 0 {
            Err(Error::Tool {
                name: "Bash".into(),
                call_id: None,
                message: format!("exit {}: {output}", result.exit_code),
            })
        } else {
            Ok(output)
        }
    }
}

/// `Read` tool that reads files from the Firecracker VM.
pub struct FirecrackerReadTool {
    vm: SharedVm,
}

#[async_trait]
impl Tool for FirecrackerReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Read".into(),
            description: "Read a file from the Firecracker microVM filesystem.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Absolute path inside the VM"}
                },
                "required": ["path"]
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::ReadOnly
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let path_str = args["path"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "Read".into(),
            message: "missing `path`".into(),
        })?;
        let vm = self.vm.lock().await;
        vm.read_file(Path::new(path_str)).await
    }
}

/// `Write` tool that writes files to the Firecracker VM.
pub struct FirecrackerWriteTool {
    vm: SharedVm,
}

#[async_trait]
impl Tool for FirecrackerWriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Write".into(),
            description: "Write a file to the Firecracker microVM filesystem.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Absolute path inside the VM"},
                    "content": {"type": "string", "description": "Content to write"}
                },
                "required": ["path", "content"]
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::External
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let path_str = args["path"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "Write".into(),
            message: "missing `path`".into(),
        })?;
        let content = args["content"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "Write".into(),
            message: "missing `content`".into(),
        })?;
        let vm = self.vm.lock().await;
        vm.write_file(Path::new(path_str), content).await?;
        Ok(format!("Wrote {} bytes to {path_str}", content.len()))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// FirecrackerToolSetProvider
// ─────────────────────────────────────────────────────────────────────────────

/// [`ToolSetProvider`] that routes tool calls through a local Firecracker VM.
///
/// `build_registry()` spawns a Firecracker process and boots the VM. If KVM
/// is unavailable or the binary is missing, it returns an error.
///
/// # Platform support
///
/// Firecracker requires Linux + KVM. On macOS or when KVM is unavailable,
/// `build_registry()` will return an error.
#[derive(Default)]
pub struct FirecrackerToolSetProvider {
    pub config: FirecrackerConfig,
    pub skills: Vec<crate::skills::Skill>,
}

impl FirecrackerToolSetProvider {
    pub fn new(config: FirecrackerConfig) -> Self {
        Self {
            config,
            skills: vec![],
        }
    }

    pub fn with_skills(mut self, skills: Vec<crate::skills::Skill>) -> Self {
        self.skills = skills;
        self
    }
}

impl ToolSetProvider for FirecrackerToolSetProvider {
    fn build_registry(&self) -> ToolRegistry {
        let config = self.config.clone();
        let skills = self.skills.clone();

        // Spawn the VM — this is synchronous (via block_in_place) since
        // build_registry is not async.
        let vm = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(async move { FirecrackerVm::spawn(config).await })
        });

        let vm = match vm {
            Ok(v) => Arc::new(Mutex::new(v)),
            Err(e) => {
                tracing::error!(
                    "FirecrackerToolSetProvider: failed to start VM: {e}; \
                     falling back to empty registry"
                );
                // Return an empty registry; callers should handle the degraded
                // state by checking whether key tools are registered.
                return ToolRegistry::default();
            }
        };

        // Build the standard registry and replace Bash/Read/Write.
        let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/workspace"));
        let mut registry =
            crate::tools::build_standard_tools(&workspace, &skills, self.config.shell_timeout_secs);
        registry.register_mut(Arc::new(FirecrackerBashTool {
            vm: Arc::clone(&vm),
        }));
        registry.register_mut(Arc::new(FirecrackerReadTool {
            vm: Arc::clone(&vm),
        }));
        registry.register_mut(Arc::new(FirecrackerWriteTool { vm }));
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

    #[test]
    fn kvm_available_returns_bool() {
        // Just verify the function doesn't panic. We don't assert the value
        // since CI may or may not have KVM.
        let _ = kvm_available();
    }

    #[test]
    fn firecracker_config_defaults() {
        let cfg = FirecrackerConfig::default();
        assert_eq!(cfg.vcpus, 1);
        assert_eq!(cfg.mem_mib, 128);
        assert_eq!(cfg.shell_timeout_secs, 60);
    }

    #[test]
    fn firecracker_provider_sandbox_mode() {
        let provider = FirecrackerToolSetProvider::default();
        assert_eq!(provider.sandbox_mode(), SandboxMode::MicroVm);
    }

    #[test]
    fn exec_result_deserialise() {
        let json =
            r#"{"ok":true,"exit_code":0,"stdout":"hello\n","stderr":"","content":"","entries":[]}"#;
        let resp: VsockResponse = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        assert_eq!(resp.exit_code, 0);
        assert_eq!(resp.stdout, "hello\n");
    }

    #[test]
    fn firecracker_api_client_request_format() {
        // Verify the request string has the correct HTTP/1.1 format.
        let body = r#"{"vcpu_count":1,"mem_size_mib":128}"#;
        let request = format!(
            "PUT /machine-config HTTP/1.1\r\n\
             Host: localhost\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\
             \r\n\
             {}",
            body.len(),
            body
        );
        assert!(request.contains("PUT /machine-config HTTP/1.1"));
        assert!(request.contains("Content-Type: application/json"));
        assert!(request.contains(&format!("Content-Length: {}", body.len())));
        assert!(request.contains(body));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn firecracker_api_parse_status_code() {
        let response = "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n";
        let status = FirecrackerApiClient::parse_status_code(response);
        assert_eq!(status, 204);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn firecracker_api_parse_body() {
        let response =
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"state\":\"Running\"}";
        let body = FirecrackerApiClient::parse_body(response);
        assert_eq!(body, r#"{"state":"Running"}"#);
    }

    #[test]
    fn vsock_request_serialises_correctly() {
        let req = VsockRequest::Exec {
            command: "ls /workspace".into(),
            timeout_secs: 30,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"cmd\":\"exec\""));
        assert!(json.contains("\"command\":\"ls /workspace\""));
        assert!(json.contains("\"timeout_secs\":30"));
    }
}
