//! Transport abstraction: decouple tools from direct filesystem/shell access.
//!
//! The `ToolTransport` trait lets tools delegate I/O to a pluggable backend.
//! The default `LocalTransport` calls `tokio::fs` / `tokio::process` directly.
//! A mock transport can be injected in tests to avoid touching the real disk.
//!
//! # SSH Transport
//!
//! `SshTransport` executes commands and file operations on a remote host
//! via the system `ssh` binary. No Rust SSH library needed — delegates to
//! the installed OpenSSH client.
//!
//! Host format: `user@host` or `user@host:port`.
//!
//! # Semantic contract (Goal 400 — normative for Goals 401–404)
//!
//! Every `ToolTransport` implementation MUST honour the following contract.
//! Tools may rely on it without knowing which tier (local / SSH / container /
//! microVM) they are talking to.
//!
//! 1. **写后立即可读 / write-then-read**: after `write_file` returns `Ok`,
//!    an immediately following `read_file` on the same path MUST observe the
//!    new contents. Push/pull-style remote transports for which this does not
//!    hold MUST say so in their `capabilities()` / docs, and the tool layer
//!    must then insert an explicit barrier.
//! 2. **路径语义 / path semantics**: the trait receives **environment-internal
//!    absolute paths** — i.e. paths as the model sees them inside the
//!    environment (`capabilities().path_root` is the prefix that maps them
//!    back to whatever backing store the transport uses). For
//!    [`LocalTransport`] `path_root` is the **empty path**, which means
//!    "environment paths == host paths as resolved by the caller"
//!    (`tools::resolve_within_any` output is passed through unchanged).
//!    Sandbox containment is ALWAYS checked by the caller (invariant #3)
//!    *before* a path reaches the transport; the transport never re-checks.
//! 3. **`persistent: false` 的含义**: when `capabilities().persistent` is
//!    `false`, each `exec_shell` may run in a fresh process/filesystem. Tools
//!    must not rely on cross-call state (cwd, env vars, temp files); any
//!    state that must survive between calls has to be re-established per
//!    call.

use async_trait::async_trait;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use walkdir::WalkDir;

/// Upper bound on how long we wait for the stdout/stderr reader tasks to see
/// EOF after the child process has exited (or been killed). Normally the pipe
/// write ends close immediately, but an orphaned descendant (`cmd &`,
/// `nohup`) that inherited them keeps the pipe open forever; without this
/// bound, `exec_shell` would park indefinitely on `task.await`.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Mirror of the bytes a reader task has read so far; lets us recover partial
/// output when the drain is aborted because an orphaned descendant keeps the
/// pipe write end open.
type ReadMirror = std::sync::Arc<std::sync::Mutex<Vec<u8>>>;

/// Spawn a `read_capped` reader task, mirroring bytes read so far.
fn spawn_reader<R: AsyncReadExt + Unpin + Send + 'static>(
    reader: R,
    max: usize,
) -> (tokio::task::JoinHandle<String>, ReadMirror) {
    let mirror: ReadMirror = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let m = std::sync::Arc::clone(&mirror);
    let handle = tokio::spawn(async move {
        let mut reader = reader;
        let mut buf = Vec::with_capacity(8 * 1024);
        let mut tmp = [0u8; 8 * 1024];
        loop {
            match reader.read(&mut tmp).await {
                Ok(0) => break,
                Ok(n) => {
                    if let Ok(mut m) = m.lock() {
                        m.extend_from_slice(&tmp[..n]);
                    }
                    if buf.len() + n > max {
                        let take = max.saturating_sub(buf.len());
                        buf.extend_from_slice(&tmp[..take]);
                        buf.extend_from_slice(b"\n... [output truncated]");
                        let _ = tokio::io::copy(&mut reader, &mut tokio::io::sink()).await;
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    });
    (handle, mirror)
}

/// Bounded drain of a reader task: on timeout, return whatever output it had
/// already read (mirrored) — a descendant holding the write end means EOF
/// will never arrive, so we must not wait for the task to finish.
async fn drain_with_grace(task: tokio::task::JoinHandle<String>, mirror: ReadMirror) -> String {
    match tokio::time::timeout(DRAIN_GRACE, task).await {
        Ok(Ok(out)) => out,
        Ok(Err(_)) | Err(_) => mirror
            .lock()
            .map(|buf| String::from_utf8_lossy(&buf).into_owned())
            .unwrap_or_default(),
    }
}

/// Result of reading a file.
#[derive(Debug, Clone)]
pub struct ReadResult {
    pub bytes: Vec<u8>,
}

/// Result of listing a directory entry.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
}

/// How a transport-level failure should be classified (Goal 400).
///
/// This is the vocabulary callers use to decide between "retry / change
/// strategy / report to the model". The agent must never see an
/// infrastructure outage as a code bug to fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportFailure {
    /// Transient fault: VM timeout, network hiccup, throttling. The caller
    /// may retry; the agent should not change its code.
    Retryable,
    /// Environment problem: missing tool in the image, missing path,
    /// insufficient permissions. The agent must change strategy or report
    /// back to the user.
    Environment,
    /// The command itself failed (non-zero exit etc.). This is normal
    /// feedback for the model.
    Tool,
}

/// Capabilities of an execution environment, as reported by
/// [`ToolTransport::capabilities`] (Goal 400).
///
/// Tools consult this instead of hard-coding assumptions about where I/O
/// runs, so the same tool code works across local / SSH / container /
/// microVM tiers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentCapabilities {
    /// Whether the sandbox has outbound network access.
    pub network: bool,
    /// Whether filesystem and processes persist across `exec_shell` calls.
    pub persistent: bool,
    /// The model-visible root of the environment (local = workspace,
    /// container = `/workspace` etc.). An **empty** `PathBuf` means
    /// "environment paths are the caller-resolved host paths" (local tier).
    pub path_root: PathBuf,
    /// The identity commands run as (`None` = unknown / not applicable).
    pub user: Option<String>,
    /// Tools detected inside the environment (`cargo` / `node` / `rg` /
    /// `git` …). Empty = not probed.
    pub toolchain: Vec<String>,
    /// Whether the environment supports snapshot / clone (container / VM
    /// tiers).
    pub snapshot: bool,
}

impl EnvironmentCapabilities {
    /// The semantics of [`LocalTransport`]: full network, persistent
    /// filesystem, caller-resolved host paths (empty `path_root`), no
    /// snapshot support, toolchain not probed.
    pub fn local() -> Self {
        Self {
            network: true,
            persistent: true,
            path_root: PathBuf::new(),
            user: None,
            toolchain: Vec::new(),
            snapshot: false,
        }
    }
}

impl EnvironmentCapabilities {
    /// Render the `<environment>` system-prompt segment (issue #31 §2).
    ///
    /// Contains ONLY the in-environment view (network / persistence /
    /// path_root / user / toolchain / snapshot) — never host paths or
    /// credentials. The environment segment is injected ONLY by
    /// non-local tiers; the local (sandbox=none) tier keeps the prompt
    /// byte-identical to its pre-#31 form by passing `None`/empty instead.
    pub fn render_environment_segment(&self) -> String {
        let mut s = String::from(
            "\n\n---\n\n## Environment\n\n\
             You are running inside a sandboxed execution environment:\n",
        );
        s.push_str(&format!(
            "- network access: {}\n",
            if self.network { "yes" } else { "no" }
        ));
        s.push_str(&format!(
            "- persistent filesystem across commands: {}\n",
            if self.persistent { "yes" } else { "no" }
        ));
        let root_desc: std::borrow::Cow<str> = if self.path_root.as_os_str().is_empty() {
            "the resolved workspace".into()
        } else {
            self.path_root.to_string_lossy()
        };
        s.push_str(&format!("- workspace root: {root_desc}\n"));
        if let Some(u) = &self.user {
            s.push_str(&format!("- runs as user: {u}\n"));
        }
        if !self.toolchain.is_empty() {
            s.push_str(&format!(
                "- available toolchain: {}\n",
                self.toolchain.join(", ")
            ));
        }
        s.push_str(&format!(
            "- snapshot support: {}",
            if self.snapshot { "yes" } else { "no" }
        ));
        s
    }
}

/// Result of running a shell command.
#[derive(Debug, Clone, Default)]
pub struct ExecResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// Transport-level classification of the invocation, when the transport
    /// itself failed (timeout, missing tool, …). `None` = the command ran
    /// and produced `exit_code` (the normal case for [`LocalTransport`]).
    /// Consumers (Goals 401–403) use this to annotate tool errors instead of
    /// guessing from stderr text.
    pub failure: Option<TransportFailure>,
}

/// One entry produced by [`ToolTransport::walk`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkEntry {
    /// Path of the entry **relative to the walk root** (platform separators).
    pub path: PathBuf,
    /// `true` for regular files. Symlinks are never followed (both local and
    /// remote walks prune them by contract), so a symlink counts as
    /// `is_file: false`.
    pub is_file: bool,
    /// File size in bytes (`0` when unknown — e.g. the list_dir-based
    /// default implementation cannot stat).
    pub size: u64,
}

/// Options for [`ToolTransport::walk`].
#[derive(Debug, Clone)]
pub struct WalkOptions {
    /// Maximum descent depth below `root` (`1` = direct children only).
    /// [`usize::MAX`] (the default) = unlimited.
    pub max_depth: usize,
    /// Follow symbolic links. **Explicit by contract** — tool defaults differ
    /// between platforms, so both the local and remote implementations must
    /// be told rather than rely on their defaults. Default `false`
    /// (symlink-loop safe, matches the previous in-tool `walkdir` behaviour).
    pub follow_symlinks: bool,
    /// Directory names pruned during the walk (at any depth). Default:
    /// `.git`, `target`, `node_modules` — the same set Glob/Grep are
    /// specified to ignore (Goal 402).
    pub ignore_dirs: Vec<String>,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            max_depth: usize::MAX,
            follow_symlinks: false,
            ignore_dirs: [".git", "target", "node_modules"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

/// Abstract transport for filesystem and shell operations.
///
/// Tools that need I/O (`ReadFile`, `WriteFile`, `ListDir`, `RunShell`,
/// `Glob`, `Grep`, …) call methods on this trait instead of using
/// `tokio::fs` / `tokio::process` directly. This makes them testable without
/// touching the real filesystem and lets the same tool code run against a
/// local workspace, an SSH host, or a container (Goals 400–403).
///
/// See the module-level **Semantic contract** section: it is normative for
/// every implementation.
#[async_trait]
pub trait ToolTransport: Send + Sync + std::fmt::Debug {
    /// Capabilities of this environment. The default returns the
    /// [`LocalTransport`] semantics so existing implementations and test
    /// doubles keep working until they opt in to a more precise answer.
    fn capabilities(&self) -> EnvironmentCapabilities {
        EnvironmentCapabilities::local()
    }

    /// Whether this transport executes **on the same machine as the agent
    /// process**. Fail-closed by default: `false` means "commands run
    /// somewhere else — or this implementation has not said" (container,
    /// microVM, SSH). Only [`LocalTransport`] returns `true`.
    ///
    /// A tool that cannot route its work through the transport — because it
    /// spawns a process of its own, e.g. `run_code`'s JavaScript runtime —
    /// MUST be gated on this. Registering one for a non-host transport would
    /// execute model-authored code on the host, outside the sandbox.
    fn executes_on_host(&self) -> bool {
        false
    }

    /// Destroy this environment (terminal, idempotent reclamation).
    ///
    /// Called by session teardown paths (delete / idle eviction / shutdown)
    /// exactly when the session owning this transport ends. MUST be safe to
    /// call repeatedly: the second and later calls are no-ops. Failures are
    /// the caller's to log only — destroy never resurrects a session. The
    /// default is a no-op (local / SSH tiers have nothing to reclaim).
    async fn destroy(&self) {}

    /// Read the full contents of a file at `path`.
    async fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>>;

    /// Write `contents` to a file at `path`, creating parent directories.
    async fn write_file(&self, path: &Path, contents: &[u8]) -> std::io::Result<()>;

    /// List entries in a directory at `path`.
    async fn list_dir(&self, path: &Path) -> std::io::Result<Vec<DirEntry>>;

    /// Recursively walk `root` and return every entry below it (`root` itself
    /// excluded; both files and directories, distinguishable via
    /// `WalkEntry::is_file`), relative to `root`, honouring `opts` (depth,
    /// ignored directory names). Entries are returned in an unspecified
    /// order — callers that need order sort themselves (Glob does).
    ///
    /// The default implementation is a **safety fallback built on repeated
    /// `list_dir` calls**: it is depth-limited (8 levels), reports `size: 0`,
    /// and cannot distinguish a symlink-to-file from a regular file (reports
    /// it as a file). Remote transports SHOULD override this with a single
    /// round-trip (`find` / `rg --files` in the environment) — the fallback
    /// costs O(directories) network round-trips and must not back a
    /// production remote tier.
    ///
    /// Only a failure to traverse `root` itself (missing root, no permission)
    /// returns `Err`; per-entry errors (unreadable subdirectory, vanished
    /// file) are skipped, matching the previous in-tool `walkdir` behaviour.
    async fn walk(&self, root: &Path, opts: &WalkOptions) -> std::io::Result<Vec<WalkEntry>> {
        const FALLBACK_MAX_DEPTH: usize = 8;
        let max_depth = opts.max_depth.min(FALLBACK_MAX_DEPTH);
        let ignored = |name: &str| opts.ignore_dirs.iter().any(|i| i == name);
        let mut out = Vec::new();
        let mut stack = vec![(root.to_path_buf(), 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            for entry in self.list_dir(&dir).await? {
                if ignored(&entry.name) {
                    continue;
                }
                let entry_depth = depth + 1;
                if entry_depth > max_depth {
                    continue;
                }
                let child = dir.join(&entry.name);
                if entry.is_dir && entry_depth < max_depth {
                    stack.push((child.clone(), entry_depth));
                }
                let rel = child.strip_prefix(root).unwrap_or(&child).to_path_buf();
                out.push(WalkEntry {
                    path: rel,
                    is_file: !entry.is_dir,
                    size: 0,
                });
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    /// Create a directory and all parents.
    async fn create_dir_all(&self, path: &Path) -> std::io::Result<()>;

    /// Execute a shell command in the given working directory with optional
    /// environment variables, timeout, and max output bytes.
    async fn exec_shell(
        &self,
        command: &str,
        cwd: &Path,
        env: &[(String, String)],
        timeout: Duration,
        max_output_bytes: usize,
    ) -> std::io::Result<ExecResult>;
}

// ---------------------------------------------------------------------------
// SSH Transport
// ---------------------------------------------------------------------------

/// SSH transport: executes commands and file operations on a remote host
/// via the system `ssh` binary.
///
/// # Host format
///
/// - `user@host` — default SSH port (22)
/// - `user@host:port` — custom port
///
/// # Auth
///
/// By default uses `ssh-agent` or `~/.ssh/id_*` keys. Optionally specify
/// a private key path via `key_path`.
#[derive(Debug, Clone)]
pub struct SshTransport {
    /// SSH connection string: user@host or user@host:port
    host: String,
    /// Path to private key (optional, defaults to ssh-agent)
    key_path: Option<PathBuf>,
    /// Remote workspace directory
    remote_workspace: PathBuf,
    /// SSH connect timeout
    connect_timeout: Duration,
    /// SSH command timeout
    command_timeout: Duration,
    /// When `true`, skip strict host key verification for ephemeral / throwaway
    /// hosts. MITM-unsafe: only for trusted networks or disposable sandboxes.
    /// Default `false` (secure).
    insecure_host_key_checking: bool,
}

impl SshTransport {
    /// Create a new SSH transport.
    ///
    /// `host` is in `user@host` or `user@host:port` format.
    /// `remote_workspace` is the absolute path to the workspace on the remote machine.
    pub fn new(host: impl Into<String>, remote_workspace: impl Into<PathBuf>) -> Self {
        Self {
            host: host.into(),
            key_path: None,
            remote_workspace: remote_workspace.into(),
            connect_timeout: Duration::from_secs(10),
            command_timeout: Duration::from_secs(300),
            insecure_host_key_checking: false,
        }
    }

    /// Set the path to an SSH private key.
    pub fn with_key(mut self, key_path: impl Into<PathBuf>) -> Self {
        self.key_path = Some(key_path.into());
        self
    }

    /// Set the SSH connect timeout.
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Set the SSH command timeout.
    pub fn with_command_timeout(mut self, timeout: Duration) -> Self {
        self.command_timeout = timeout;
        self
    }

    /// Opt in to skipping host-key verification (MITM-unsafe).
    /// Emits a `tracing::warn!` so the choice is visible in logs.
    ///
    /// When enabled, commands are built with `StrictHostKeyChecking=accept-new`
    /// (first-use trust): the remote host key is accepted on first connection
    /// but later changes are still detected. This is strictly better than
    /// `StrictHostKeyChecking=no` but is still unsafe against an active MITM
    /// on the very first connection — only use it for trusted networks or
    /// disposable sandboxes. Default is `false` (secure).
    pub fn with_insecure_host_key_checking(mut self, on: bool) -> Self {
        self.insecure_host_key_checking = on;
        self
    }

    /// Return the host string.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Return the key path, if set.
    pub fn key_path(&self) -> Option<&Path> {
        self.key_path.as_deref()
    }

    /// Return the remote workspace path.
    pub fn remote_workspace(&self) -> &Path {
        &self.remote_workspace
    }

    /// Build a `tokio::process::Command` for an SSH invocation.
    fn build_ssh_command(&self, remote_command: &str) -> Command {
        let mut cmd = Command::new("ssh");

        // Non-interactive options
        cmd.arg("-o").arg("BatchMode=yes");
        cmd.arg("-o")
            .arg(format!("ConnectTimeout={}", self.connect_timeout.as_secs()));

        // Host key verification is secure by default: use the user's
        // known_hosts. Only an explicit opt-in relaxes it for ephemeral hosts.
        if self.insecure_host_key_checking {
            tracing::warn!(
                host = %self.host,
                "SshTransport: host key verification relaxed to StrictHostKeyChecking=accept-new \
                 (MITM-unsafe — only for trusted networks or disposable sandboxes)"
            );
            cmd.arg("-o").arg("StrictHostKeyChecking=accept-new");
        }

        // Optional identity file
        if let Some(ref key) = self.key_path {
            cmd.arg("-i").arg(key);
        }

        // Parse host:port format
        let (host, port) = parse_host(&self.host);
        if let Some(p) = port {
            cmd.arg("-p").arg(p.to_string());
        }

        cmd.arg(&host);
        cmd.arg(remote_command);

        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        cmd
    }

    /// Execute a command via SSH and capture stdout/stderr.
    async fn ssh_exec(&self, command: &str) -> std::io::Result<ExecResult> {
        let mut child = self.build_ssh_command(command).spawn()?;

        let stdout = child.stdout.take().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stdout pipe not available")
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stderr pipe not available")
        })?;

        let max: usize = 128 * 1024;
        let (stdout_task, stdout_mirror) = spawn_reader(stdout, max);
        let (stderr_task, stderr_mirror) = spawn_reader(stderr, max);

        let wait = child.wait();
        let status = match tokio::time::timeout(self.command_timeout, wait).await {
            Ok(s) => s?,
            Err(_) => {
                // Kill the child so the SSH connection drops and the reader
                // tasks unblock (the remote command's pipes close when the
                // ssh client dies); then drain them so their JoinHandles
                // don't detach with buffered output. Order matters: the
                // readers block on the pipes until the child is dead, so
                // kill/wait BEFORE awaiting them. Best-effort cleanup on
                // the error path — swallow secondary failures from it.
                let _ = child.start_kill();
                let _ = child.wait().await;
                let _ = drain_with_grace(stdout_task, stdout_mirror).await;
                let _ = drain_with_grace(stderr_task, stderr_mirror).await;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("SSH command timed out after {:?}", self.command_timeout),
                ));
            }
        };

        let out = drain_with_grace(stdout_task, stdout_mirror).await;
        let err = drain_with_grace(stderr_task, stderr_mirror).await;
        let code = status.code();

        Ok(ExecResult {
            exit_code: code,
            stdout: out,
            stderr: err,
            failure: None,
        })
    }

    /// Write content to a remote file by piping base64 over SSH.
    async fn ssh_write_file(&self, path: &Path, contents: &[u8]) -> std::io::Result<()> {
        // First ensure the parent directory exists
        if let Some(parent) = path.parent() {
            let mkdir_cmd = format!(
                "mkdir -p {}",
                shell_escape(parent.to_string_lossy().as_ref())
            );
            let result = self.ssh_exec(&mkdir_cmd).await?;
            if result.exit_code != Some(0) {
                return Err(std::io::Error::other(format!(
                    "mkdir failed on remote: {}",
                    result.stderr
                )));
            }
        }

        // Write file content via base64 to avoid escaping issues
        let b64 = base64_encode(contents);
        let write_cmd = format!(
            "echo '{}' | base64 -d > {}",
            b64,
            shell_escape(path.to_string_lossy().as_ref())
        );
        let result = self.ssh_exec(&write_cmd).await?;
        if result.exit_code != Some(0) {
            return Err(std::io::Error::other(format!(
                "write failed on remote: {}",
                result.stderr
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl ToolTransport for SshTransport {
    async fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        let cmd = format!("cat {}", shell_escape(path.to_string_lossy().as_ref()));
        let result = self.ssh_exec(&cmd).await?;
        if result.exit_code != Some(0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "remote file not found: {}: {}",
                    path.display(),
                    result.stderr
                ),
            ));
        }
        Ok(result.stdout.into_bytes())
    }

    async fn write_file(&self, path: &Path, contents: &[u8]) -> std::io::Result<()> {
        self.ssh_write_file(path, contents).await
    }

    async fn list_dir(&self, path: &Path) -> std::io::Result<Vec<DirEntry>> {
        let cmd = format!("ls -1a {}", shell_escape(path.to_string_lossy().as_ref()));
        let result = self.ssh_exec(&cmd).await?;
        if result.exit_code != Some(0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "remote dir not found: {}: {}",
                    path.display(),
                    result.stderr
                ),
            ));
        }

        let mut entries = Vec::new();
        for name in result.stdout.lines() {
            let name = name.trim();
            if name.is_empty() || name == "." || name == ".." {
                continue;
            }
            // Check if it's a directory via a separate SSH call
            let test_cmd = format!(
                "test -d {} && echo dir || echo file",
                shell_escape(&format!("{}/{}", path.to_string_lossy(), name))
            );
            let is_dir = self
                .ssh_exec(&test_cmd)
                .await
                .ok()
                .map(|r| r.stdout.trim() == "dir")
                .unwrap_or(false);
            entries.push(DirEntry {
                name: name.to_string(),
                is_dir,
            });
        }

        Ok(entries)
    }

    async fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
        let cmd = format!("mkdir -p {}", shell_escape(path.to_string_lossy().as_ref()));
        let result = self.ssh_exec(&cmd).await?;
        if result.exit_code != Some(0) {
            return Err(std::io::Error::other(format!(
                "mkdir failed on remote: {}",
                result.stderr
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
        // Build the remote command: cd to workspace, set env vars, run command
        let mut env_prefix = String::new();
        for (key, val) in env {
            env_prefix.push_str(&format!("{}={} ", key, shell_escape(val)));
        }

        let remote_cmd = format!(
            "cd {} && {} {}",
            shell_escape(cwd.to_string_lossy().as_ref()),
            env_prefix,
            command
        );

        let mut child = self.build_ssh_command(&remote_cmd).spawn()?;

        let stdout = child.stdout.take().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stdout pipe not available")
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stderr pipe not available")
        })?;

        let max = max_output_bytes;
        let (stdout_task, stdout_mirror) = spawn_reader(stdout, max);
        let (stderr_task, stderr_mirror) = spawn_reader(stderr, max);

        let wait = child.wait();
        let status = match tokio::time::timeout(timeout, wait).await {
            Ok(s) => s?,
            Err(_) => {
                // Same kill-then-drain cleanup as `ssh_exec`: the ssh
                // client must die before the reader tasks can unblock,
                // and awaiting them here prevents their JoinHandles from
                // detaching with buffered output.
                let _ = child.start_kill();
                let _ = child.wait().await;
                let _ = drain_with_grace(stdout_task, stdout_mirror).await;
                let _ = drain_with_grace(stderr_task, stderr_mirror).await;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("SSH command timed out after {:?}", timeout),
                ));
            }
        };

        let out = drain_with_grace(stdout_task, stdout_mirror).await;
        let err = drain_with_grace(stderr_task, stderr_mirror).await;
        let code = status.code();

        Ok(ExecResult {
            exit_code: code,
            stdout: out,
            stderr: err,
            failure: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Local Transport
// ---------------------------------------------------------------------------

/// The default transport that performs real I/O via `tokio::fs` and
/// `tokio::process`.
#[derive(Debug, Clone, Default)]
pub struct LocalTransport;

/// Issue #89: is `name` a credential-bearing environment variable that the
/// default (`RECURSIVE_SANDBOX` unset / `none` / `policy`) tier must **not**
/// hand to a child shell?
///
/// Without a scrub the child inherits the service process's whole env, so a
/// prompt-injected `printenv` ships the upstream LLM key
/// (`RECURSIVE_API_KEY`), the inbound HTTP auth keys
/// (`RECURSIVE_HTTP_AUTH_KEYS`) and the JWT signing secret
/// (`RECURSIVE_HTTP_AUTH_JWT_SECRET`) straight into the plaintext transcript.
///
/// The `RECURSIVE_` namespace carries this process's own credentials; the
/// generic tokens catch third-party secrets (`OPENAI_API_KEY`,
/// `AWS_SECRET_ACCESS_KEY`, `GITHUB_TOKEN`, …) a deployment may have
/// exported. `AUTH` is matched only as a whole `_`-delimited segment so a
/// benign `GIT_AUTHOR_NAME` survives.
fn is_sensitive_env_var(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if upper.starts_with("RECURSIVE_") {
        return true;
    }
    upper.contains("KEY")
        || upper.contains("SECRET")
        || upper.contains("TOKEN")
        || upper.contains("PASSWORD")
        || upper.contains("PASSWD")
        || upper.contains("CREDENTIAL")
        || upper.split('_').any(|segment| segment == "AUTH")
}

/// Issue #89: apply the default-tier credential scrub to a child command —
/// clear the inherited env, then re-add only the non-sensitive host vars
/// (PATH/HOME/toolchain/…) so the local dev loop keeps working. Shared by
/// [`LocalTransport::exec_shell`] and `run_background`'s host path; the
/// caller layers any explicit `env` pairs on top afterwards.
pub(crate) fn scrub_child_env(cmd: &mut Command) {
    cmd.env_clear();
    for (key, val) in std::env::vars_os() {
        if !is_sensitive_env_var(&key.to_string_lossy()) {
            cmd.env(key, val);
        }
    }
}

#[async_trait]
impl ToolTransport for LocalTransport {
    /// The one transport whose commands run in this process's own
    /// environment — the only tier where a host-spawning tool is honest.
    fn executes_on_host(&self) -> bool {
        true
    }

    async fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        tokio::fs::read(path).await
    }

    async fn write_file(&self, path: &Path, contents: &[u8]) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            self.create_dir_all(parent).await?;
        }
        tokio::fs::write(path, contents).await
    }

    async fn list_dir(&self, path: &Path) -> std::io::Result<Vec<DirEntry>> {
        let mut read_dir = tokio::fs::read_dir(path).await?;
        let mut entries = Vec::new();
        while let Some(entry) = read_dir.next_entry().await? {
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
            entries.push(DirEntry { name, is_dir });
        }
        Ok(entries)
    }

    /// Single-pass recursive walk via `walkdir` (same crate the tools used
    /// before Goal 402, so traversal order and filtering semantics are
    /// byte-identical to the pre-transport behaviour).
    ///
    /// Traversal only (missing root) errors return `Err`; per-entry errors
    /// are skipped, exactly like the tools' previous
    /// `WalkDir … filter_map(|e| e.ok())` pipeline.
    async fn walk(&self, root: &Path, opts: &WalkOptions) -> std::io::Result<Vec<WalkEntry>> {
        if !root.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("walk root not found: {}", root.display()),
            ));
        }
        let mut out = Vec::new();
        for entry in WalkDir::new(root)
            // Explicit by contract (Goal 402): never rely on walkdir's
            // default — the remote implementation must match this policy.
            .follow_links(opts.follow_symlinks)
            .max_depth(opts.max_depth)
            .into_iter()
            .filter_entry(|e| {
                // Prune ignored dirs at any depth below the root; the root
                // itself (depth 0) is never pruned so scoping *into* an
                // ignored dir still works.
                e.depth() == 0
                    || !e
                        .file_name()
                        .to_str()
                        .map(|name| opts.ignore_dirs.iter().any(|i| i == name))
                        .unwrap_or(false)
            })
        {
            let Ok(entry) = entry else { continue };
            let is_root = entry.depth() == 0;
            if is_root && !entry.file_type().is_file() {
                // The root directory itself is not part of the result.
                // (walkdir yields it at depth 0; the old in-tool pipelines
                // filtered it out via `file_type().is_file()`.)
                continue;
            }
            let path = entry.path().strip_prefix(root).unwrap_or(entry.path());
            out.push(WalkEntry {
                path: path.to_path_buf(),
                is_file: entry.file_type().is_file(),
                size: entry.metadata().map(|m| m.len()).unwrap_or(0),
            });
        }
        Ok(out)
    }

    async fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
        tokio::fs::create_dir_all(path).await
    }

    async fn exec_shell(
        &self,
        command: &str,
        cwd: &Path,
        env: &[(String, String)],
        timeout: Duration,
        max_output_bytes: usize,
    ) -> std::io::Result<ExecResult> {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(command);
        cmd.current_dir(cwd);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        // Defence in depth against orphan processes on timeout: Tokio's
        // `Child` defaults to `kill_on_drop = false`, so a bare `return Err`
        // in the timeout branch would leave the shell and any of its
        // descendants running after the error is surfaced.
        cmd.kill_on_drop(true);

        // Issue #89: the default tier must not leak the service process's
        // credentials into LLM-authored commands — scrub the inherited env,
        // then layer the tool call's explicit pairs on top so they win.
        scrub_child_env(&mut cmd);
        for (key, val) in env {
            cmd.env(key, val);
        }

        let mut child = cmd.spawn()?;

        let stdout = child.stdout.take().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stdout pipe not available")
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stderr pipe not available")
        })?;

        let max = max_output_bytes;
        let (stdout_task, stdout_mirror) = spawn_reader(stdout, max);
        let (stderr_task, stderr_mirror) = spawn_reader(stderr, max);

        let wait = child.wait();
        let status = match tokio::time::timeout(timeout, wait).await {
            Ok(s) => s?,
            Err(_) => {
                // Best-effort SIGKILL of the timed-out process; then drain
                // both reader tasks through the same bounded grace as the
                // SSH arms — an orphaned descendant (`cmd &`, `nohup`) that
                // inherited the pipe write ends keeps EOF from ever arriving,
                // and the reader tasks would otherwise park forever (issue
                // #47①). `kill_on_drop(true)` set at spawn is the safety net
                // for any other early-exit path.
                let _ = child.start_kill();
                let _ = child.wait().await;
                let _ = drain_with_grace(stdout_task, stdout_mirror).await;
                let _ = drain_with_grace(stderr_task, stderr_mirror).await;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("command timed out after {:?}", timeout),
                ));
            }
        };

        let out = drain_with_grace(stdout_task, stdout_mirror).await;
        let err = drain_with_grace(stderr_task, stderr_mirror).await;
        let code = status.code();

        Ok(ExecResult {
            exit_code: code,
            stdout: out,
            stderr: err,
            failure: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Goal 401/402 convention for consuming transport failures from the
/// `std::io::Result`-shaped methods (`read_file` / `write_file` / `walk`):
/// error kinds that mark a **transient** fault (timeout, connection loss)
/// must be surfaced to the model with a `retryable: ` prefix so it knows the
/// failure is infrastructure, not a code bug to fix. Exec-style failures are
/// classified structurally via [`ExecResult::failure`] instead.
pub fn is_retryable_io_error(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::WouldBlock
    )
}

/// Prefix for a tool-error message per [`is_retryable_io_error`].
pub fn retryable_prefix(e: &std::io::Error) -> &'static str {
    if is_retryable_io_error(e) {
        "retryable: "
    } else {
        ""
    }
}

/// Parse a host string of the form `user@host` or `user@host:port`.
/// Returns `(host_string, optional_port)`.
fn parse_host(host: &str) -> (String, Option<u16>) {
    // Split off port if present (last colon after @)
    if let Some(at_pos) = host.rfind('@') {
        let after_at = &host[at_pos + 1..];
        if let Some(colon_pos) = after_at.rfind(':') {
            let host_part = format!("{}@{}", &host[..at_pos], &after_at[..colon_pos]);
            let port: u16 = after_at[colon_pos + 1..].parse().unwrap_or(22);
            return (host_part, Some(port));
        }
    } else {
        // No @ — just host or host:port
        if let Some(colon_pos) = host.rfind(':') {
            let host_part = host[..colon_pos].to_string();
            let port: u16 = host[colon_pos + 1..].parse().unwrap_or(22);
            return (host_part, Some(port));
        }
    }
    (host.to_string(), None)
}

/// Shell-escape a string for safe use in SSH commands.
/// Wraps in single quotes and escapes any single quotes inside.
fn shell_escape(s: &str) -> String {
    let escaped = s.replace('\'', "'\\''");
    format!("'{}'", escaped)
}

/// Base64-encode bytes (simple implementation without external crate).
fn base64_encode(bytes: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;

        result.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        result.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);

        if chunk.len() > 1 {
            result.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }

        if chunk.len() > 2 {
            result.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[cfg(not(target_os = "windows"))]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // --- Local transport tests (unchanged) ---

    #[tokio::test]
    async fn local_transport_read_write_roundtrip() {
        let t = LocalTransport;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("hello.txt");

        t.write_file(&path, b"world").await.unwrap();
        let data = t.read_file(&path).await.unwrap();
        assert_eq!(data, b"world");
    }

    #[tokio::test]
    async fn local_transport_list_dir() {
        let t = LocalTransport;
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "x").unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();

        let entries = t.list_dir(tmp.path()).await.unwrap();
        let mut names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["a.txt", "sub"]);
        assert!(!entries.iter().any(|e| e.name == "a.txt" && e.is_dir));
        assert!(entries.iter().any(|e| e.name == "sub" && e.is_dir));
    }

    #[tokio::test]
    async fn local_transport_exec_shell() {
        let t = LocalTransport;
        let tmp = TempDir::new().unwrap();
        let result = t
            .exec_shell(
                "echo hello",
                tmp.path(),
                &[],
                Duration::from_secs(5),
                128 * 1024,
            )
            .await
            .unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert!(result.stdout.contains("hello"));
    }

    #[tokio::test]
    async fn local_transport_exec_shell_with_env() {
        let t = LocalTransport;
        let tmp = TempDir::new().unwrap();
        let result = t
            .exec_shell(
                "echo $MY_VAR",
                tmp.path(),
                &[("MY_VAR".into(), "test_value".into())],
                Duration::from_secs(5),
                128 * 1024,
            )
            .await
            .unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert!(result.stdout.contains("test_value"));
    }

    /// Issue #89: the default (`none`) tier must not hand the service
    /// process's credentials to LLM-authored commands — a prompt-injected
    /// `printenv` used to ship the upstream LLM key / inbound HTTP auth
    /// keys / JWT signing secret straight into the plaintext transcript.
    ///
    /// Env-var checks are consolidated into ONE test (see `.dev/AGENTS.md`):
    /// `set_var` is process-global and `cargo test` runs tests in parallel.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // std env lock is fine: only same-crate tests contend
    async fn local_transport_exec_shell_scrubs_sensitive_env() {
        let _lock = crate::test_util::env_lock();

        // Names the scrub must drop (RECURSIVE_ namespace + generic
        // credential patterns).
        let secret_names = [
            "RECURSIVE_API_KEY",
            "RECURSIVE_HTTP_AUTH_KEYS",
            "RECURSIVE_HTTP_AUTH_JWT_SECRET",
            "OPENAI_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "DB_PASSWORD",
            "SSH_AUTH_SOCK",
        ];
        // Names the scrub must keep — the local dev loop needs PATH/HOME/
        // toolchain, and `GIT_AUTHOR_NAME` pins that the `AUTH` segment
        // match does not swallow `AUTHOR`.
        let benign = [
            ("CARGO_TEST_BENIGN_VAR", "keepme"),
            ("GIT_AUTHOR_NAME", "Ada"),
        ];

        let saved: Vec<(String, Option<std::ffi::OsString>)> = secret_names
            .iter()
            .map(|k| ((*k).to_string(), std::env::var_os(k)))
            .chain(
                benign
                    .iter()
                    .map(|(k, _)| ((*k).to_string(), std::env::var_os(k))),
            )
            .collect();

        // SAFETY: process-global env mutation, serialised by `env_lock`.
        for k in secret_names {
            unsafe { std::env::set_var(k, "leak-me") };
        }
        for (k, v) in benign {
            unsafe { std::env::set_var(k, v) };
        }

        let t = LocalTransport;
        let tmp = TempDir::new().unwrap();

        // Literal acceptance from issue #89: `printenv | grep -c RECURSIVE`
        // must be 0 in a default-tier session.
        let count = t
            .exec_shell(
                "printenv | grep -c RECURSIVE",
                tmp.path(),
                &[],
                Duration::from_secs(5),
                128 * 1024,
            )
            .await
            .unwrap();
        assert_eq!(count.stdout.trim(), "0", "leaked RECURSIVE_* env");

        let out = t
            .exec_shell(
                "printenv",
                tmp.path(),
                &[],
                Duration::from_secs(5),
                256 * 1024,
            )
            .await
            .unwrap();
        for name in secret_names {
            assert!(
                !out.stdout.contains(&format!("{name}=")),
                "sensitive env `{name}` leaked to the child shell"
            );
        }
        for (k, v) in benign {
            let expected = format!("{k}={v}");
            assert!(
                out.stdout.lines().any(|l| l == expected.as_str()),
                "benign env `{k}` was scrubbed from the child shell"
            );
        }

        // Restore the process env for the rest of the suite.
        for (k, prev) in saved {
            unsafe {
                match prev {
                    Some(v) => std::env::set_var(&k, v),
                    None => std::env::remove_var(&k),
                }
            }
        }
    }

    #[tokio::test]
    async fn local_transport_exec_shell_timeout() {
        let t = LocalTransport;
        let tmp = TempDir::new().unwrap();
        let result = t
            .exec_shell(
                "sleep 10",
                tmp.path(),
                &[],
                Duration::from_millis(100),
                128 * 1024,
            )
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    }

    // TODO(goal-360): the SSH timeout arms in `ssh_exec` and `exec_shell`
    // now kill the child and drain both reader tasks before returning,
    // but that behavior can't be exercised end-to-end here without an SSH
    // server in the test environment — spawning `ssh user@host` against a
    // missing server fails fast with connection refused and never reaches
    // the timeout branch. The kill-then-drain contract is pinned by
    // `shell_timeout_drains_reader_tasks` in shell.rs (same spawn → two
    // reader tasks → timeout(child.wait()) structure) and is
    // grep-verifiable in this file: the timeout `Err(_)` arms contain
    // `start_kill` and `stdout_task.await`.

    #[tokio::test]
    async fn local_transport_create_dir_all() {
        let t = LocalTransport;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("a").join("b").join("c");
        t.create_dir_all(&path).await.unwrap();
        assert!(path.exists());
        assert!(path.is_dir());
    }

    // --- Goal 400: capabilities + failure classification ---

    /// A transport that only implements the required methods (no
    /// `capabilities` / `walk` overrides), used to pin the default trait
    /// implementations.
    #[derive(Debug)]
    struct BareTransport(LocalTransport);

    #[async_trait]
    impl ToolTransport for BareTransport {
        async fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            self.0.read_file(path).await
        }
        async fn write_file(&self, path: &Path, contents: &[u8]) -> std::io::Result<()> {
            self.0.write_file(path, contents).await
        }
        async fn list_dir(&self, path: &Path) -> std::io::Result<Vec<DirEntry>> {
            self.0.list_dir(path).await
        }
        async fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
            self.0.create_dir_all(path).await
        }
        async fn exec_shell(
            &self,
            command: &str,
            cwd: &Path,
            env: &[(String, String)],
            timeout: Duration,
            max_output_bytes: usize,
        ) -> std::io::Result<ExecResult> {
            self.0
                .exec_shell(command, cwd, env, timeout, max_output_bytes)
                .await
        }
    }

    #[test]
    fn local_transport_capabilities_match_local_semantics() {
        let caps = LocalTransport.capabilities();
        assert!(caps.network, "local tier has outbound network");
        assert!(caps.persistent, "local tier persists across execs");
        assert!(
            caps.path_root.as_os_str().is_empty(),
            "empty path_root = caller-resolved host paths (path semantics contract)"
        );
        assert_eq!(caps.user, None);
        assert!(caps.toolchain.is_empty(), "toolchain not probed locally");
        assert!(!caps.snapshot, "local tier cannot snapshot");
        assert_eq!(caps, EnvironmentCapabilities::local());
    }

    #[test]
    fn capabilities_default_is_local_semantics() {
        // Test doubles that don't opt in keep working (Goal 400 requirement).
        let caps = BareTransport(LocalTransport).capabilities();
        assert_eq!(caps, EnvironmentCapabilities::local());
    }

    /// Issue #134: host execution is opt-in and fail-closed. Only the local
    /// tier may permit a tool that spawns a process of its own (`run_code`);
    /// everything else — container, microVM, SSH, or a transport that simply
    /// has not said — is treated as sandboxed.
    #[test]
    fn host_execution_is_opt_in_and_fail_closed() {
        assert!(
            LocalTransport.executes_on_host(),
            "the local tier runs commands in this process's environment"
        );
        assert!(
            !BareTransport(LocalTransport).executes_on_host(),
            "a transport that has not opted in must not be treated as the host"
        );
    }

    #[test]
    fn exec_result_default_has_no_failure() {
        let r = ExecResult::default();
        assert_eq!(r.exit_code, None);
        assert_eq!(r.failure, None);
    }

    #[test]
    fn transport_failure_variants_are_distinguishable() {
        // The classification must be transferable and comparable (Goal 400:
        // consumers in 401–403 branch on the value).
        let retryable = ExecResult {
            failure: Some(TransportFailure::Retryable),
            ..ExecResult::default()
        };
        let environment = ExecResult {
            failure: Some(TransportFailure::Environment),
            ..ExecResult::default()
        };
        let tool = ExecResult {
            failure: Some(TransportFailure::Tool),
            ..ExecResult::default()
        };
        assert_ne!(retryable.failure, environment.failure);
        assert_ne!(environment.failure, tool.failure);
        assert_ne!(retryable.failure, tool.failure);
        assert_eq!(retryable.failure, Some(TransportFailure::Retryable));
    }

    /// A mock transport that always reports a retryable failure from
    /// `exec_shell` — pins that the classification travels through the trait
    /// to the caller.
    #[derive(Debug)]
    struct FlakyTransport;

    #[async_trait]
    impl ToolTransport for FlakyTransport {
        async fn read_file(&self, _path: &Path) -> std::io::Result<Vec<u8>> {
            Ok(Vec::new())
        }
        async fn write_file(&self, _path: &Path, _contents: &[u8]) -> std::io::Result<()> {
            Ok(())
        }
        async fn list_dir(&self, _path: &Path) -> std::io::Result<Vec<DirEntry>> {
            Ok(Vec::new())
        }
        async fn create_dir_all(&self, _path: &Path) -> std::io::Result<()> {
            Ok(())
        }
        async fn exec_shell(
            &self,
            _command: &str,
            _cwd: &Path,
            _env: &[(String, String)],
            _timeout: Duration,
            _max_output_bytes: usize,
        ) -> std::io::Result<ExecResult> {
            Ok(ExecResult {
                exit_code: None,
                stdout: String::new(),
                stderr: "vm timed out".into(),
                failure: Some(TransportFailure::Retryable),
            })
        }
    }

    #[tokio::test]
    async fn mock_transport_surfaces_failure_classification() {
        let r = FlakyTransport
            .exec_shell("true", Path::new("/"), &[], Duration::from_secs(1), 1024)
            .await
            .unwrap();
        assert_eq!(r.failure, Some(TransportFailure::Retryable));
    }

    // --- Goal 402: walk ---

    fn write_tree(root: &std::path::Path) {
        for rel in [
            "a.txt",
            "src/b.rs",
            "src/deep/c.rs",
            "target/d.rs",
            ".git/config",
            "node_modules/pkg/e.js",
        ] {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "x").unwrap();
        }
    }

    #[tokio::test]
    async fn local_transport_walk_returns_relative_paths_with_metadata() {
        let t = LocalTransport;
        let tmp = TempDir::new().unwrap();
        write_tree(tmp.path());

        let entries = t.walk(tmp.path(), &WalkOptions::default()).await.unwrap();
        let files: Vec<String> = entries
            .iter()
            .filter(|e| e.is_file)
            .map(|e| e.path.to_string_lossy().into_owned())
            .collect();
        for expected in ["a.txt", "src/b.rs", "src/deep/c.rs"] {
            assert!(
                files.iter().any(|f| f.ends_with(expected)),
                "walk must find {expected} (got {files:?})"
            );
        }
        // All paths are relative to the walk root.
        assert!(
            entries
                .iter()
                .all(|e| !e.path.is_absolute() && !e.path.starts_with(tmp.path())),
            "walk entries must be root-relative"
        );
        // Sizes are real (walkdir metadata), not the fallback's 0.
        let a = entries
            .iter()
            .find(|e| e.path.file_name().unwrap() == "a.txt")
            .unwrap();
        assert_eq!(a.size, 1);
    }

    #[tokio::test]
    async fn local_transport_walk_prunes_ignored_dirs() {
        let t = LocalTransport;
        let tmp = TempDir::new().unwrap();
        write_tree(tmp.path());

        let entries = t.walk(tmp.path(), &WalkOptions::default()).await.unwrap();
        let names: Vec<String> = entries
            .iter()
            .map(|e| e.path.to_string_lossy().into_owned())
            .collect();
        assert!(
            !names.iter().any(|n| n.contains(".git") || n.contains("target") || n.contains("node_modules")),
            ".git / target / node_modules must be pruned on both local and remote paths (got {names:?})"
        );
    }

    #[tokio::test]
    async fn local_transport_walk_respects_max_depth() {
        let t = LocalTransport;
        let tmp = TempDir::new().unwrap();
        write_tree(tmp.path());

        let opts = WalkOptions {
            max_depth: 1,
            ..WalkOptions::default()
        };
        let entries = t.walk(tmp.path(), &opts).await.unwrap();
        let files: Vec<String> = entries
            .iter()
            .filter(|e| e.is_file)
            .map(|e| e.path.to_string_lossy().into_owned())
            .collect();
        assert!(files.iter().any(|f| f == "a.txt"), "depth-1 file present");
        assert!(
            !files.iter().any(|f| f.contains("b.rs")),
            "depth-2 files excluded at max_depth 1"
        );
    }

    #[tokio::test]
    async fn local_transport_walk_missing_root_is_error() {
        let t = LocalTransport;
        let err = t
            .walk(Path::new("/nonexistent/walk/root"), &WalkOptions::default())
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_transport_walk_does_not_follow_symlinks_by_default() {
        let t = LocalTransport;
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("real.txt"), "x").unwrap();
        std::os::unix::fs::symlink(tmp.path().join("real.txt"), tmp.path().join("link.txt"))
            .unwrap();

        let entries = t.walk(tmp.path(), &WalkOptions::default()).await.unwrap();
        let link = entries
            .iter()
            .find(|e| e.path.file_name().unwrap() == "link.txt")
            .expect("symlink must appear as an entry");
        assert!(
            !link.is_file,
            "with follow_symlinks=false (explicit default) a symlink is not a file"
        );
    }

    #[test]
    fn walk_options_defaults_pin_the_contract() {
        let opts = WalkOptions::default();
        assert!(
            !opts.follow_symlinks,
            "symlink policy must be explicit and default to not following (Goal 402 trap)"
        );
        assert_eq!(opts.max_depth, usize::MAX, "default walk is unbounded");
        for dir in [".git", "target", "node_modules"] {
            assert!(
                opts.ignore_dirs.iter().any(|i| i == dir),
                "{dir} must be in the default ignore set"
            );
        }
    }

    #[tokio::test]
    async fn default_walk_fallback_is_depth_limited_and_sorted() {
        let t = BareTransport(LocalTransport);
        let tmp = TempDir::new().unwrap();
        // Build a chain 12 levels deep; the fallback caps at 8.
        let mut deep = tmp.path().to_path_buf();
        for i in 0..12 {
            deep = deep.join(format!("d{i}"));
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("bottom.txt"), "x").unwrap();
        std::fs::write(tmp.path().join("top.txt"), "x").unwrap();

        let entries = t.walk(tmp.path(), &WalkOptions::default()).await.unwrap();
        let names: Vec<String> = entries
            .iter()
            .map(|e| e.path.to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().any(|n| n == "top.txt"),
            "shallow files found by fallback"
        );
        assert!(
            !names.iter().any(|n| n.contains("bottom.txt")),
            "fallback must be depth-limited (remote tiers must override walk)"
        );
        // Fallback reports size 0 (no stat through list_dir).
        let top = names.iter().find(|n| n.as_str() == "top.txt").unwrap();
        let entry = entries
            .iter()
            .find(|e| e.path.to_string_lossy() == *top)
            .unwrap();
        assert_eq!(entry.size, 0);
        // Fallback output is sorted by relative path.
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    #[tokio::test]
    async fn default_walk_fallback_prunes_ignored_dirs() {
        let t = BareTransport(LocalTransport);
        let tmp = TempDir::new().unwrap();
        write_tree(tmp.path());

        let entries = t.walk(tmp.path(), &WalkOptions::default()).await.unwrap();
        let names: Vec<String> = entries
            .iter()
            .map(|e| e.path.to_string_lossy().into_owned())
            .collect();
        assert!(
            !names
                .iter()
                .any(|n| n.contains(".git") || n.contains("target") || n.contains("node_modules")),
            "both walk implementations must share ignore semantics (got {names:?})"
        );
        assert!(names.iter().any(|n| n == "a.txt"));
        assert!(names.iter().any(|n| n.ends_with("b.rs")));
    }

    // --- SSH transport tests (no actual SSH required) ---

    #[test]
    fn ssh_transport_constructor() {
        let t = SshTransport::new("user@host", "/remote/workspace");
        assert_eq!(t.host(), "user@host");
        assert_eq!(t.remote_workspace(), Path::new("/remote/workspace"));
        assert!(t.key_path().is_none());
    }

    #[test]
    fn ssh_transport_with_key() {
        let t = SshTransport::new("user@host", "/remote/workspace").with_key("/path/to/key");
        assert_eq!(t.key_path(), Some(Path::new("/path/to/key")));
    }

    #[test]
    fn ssh_transport_with_timeouts() {
        let t = SshTransport::new("user@host", "/remote/workspace")
            .with_connect_timeout(Duration::from_secs(30))
            .with_command_timeout(Duration::from_secs(600));
        // We can't inspect private fields directly, but we can verify
        // the builder pattern compiles and returns the right type.
        assert_eq!(t.host(), "user@host");
    }

    #[test]
    fn ssh_build_command_basic() {
        let t = SshTransport::new("user@host", "/remote/workspace");
        let cmd = t.build_ssh_command("ls -la");
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();

        // Should include options, host, and command
        assert!(args.contains(&"user@host".to_string()));
        assert!(args.contains(&"ls -la".to_string()));
        // Should have BatchMode=yes
        assert!(args.contains(&"-o".to_string()));
        assert!(args.contains(&"BatchMode=yes".to_string()));
        // Default must NOT disable host key verification
        assert!(!args.contains(&"StrictHostKeyChecking=no".to_string()));
    }

    #[test]
    fn ssh_build_command_with_key() {
        let t =
            SshTransport::new("user@host", "/remote/workspace").with_key("/home/user/.ssh/id_rsa");
        let cmd = t.build_ssh_command("echo test");
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();

        assert!(args.contains(&"-i".to_string()));
        assert!(args.contains(&"/home/user/.ssh/id_rsa".to_string()));
    }

    #[test]
    fn ssh_build_command_with_port() {
        let t = SshTransport::new("user@host:2222", "/remote/workspace");
        let cmd = t.build_ssh_command("whoami");
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();

        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"2222".to_string()));
        assert!(args.contains(&"user@host".to_string()));
    }

    #[test]
    fn ssh_build_command_without_user() {
        // Host without @ — just a hostname
        let t = SshTransport::new("remote-server:2222", "/remote/workspace");
        let cmd = t.build_ssh_command("whoami");
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();

        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"2222".to_string()));
        assert!(args.contains(&"remote-server".to_string()));
    }

    #[test]
    fn parse_host_user_at_host() {
        let (host, port) = parse_host("user@host");
        assert_eq!(host, "user@host");
        assert_eq!(port, None);
    }

    #[test]
    fn parse_host_user_at_host_port() {
        let (host, port) = parse_host("user@host:2222");
        assert_eq!(host, "user@host");
        assert_eq!(port, Some(2222));
    }

    #[test]
    fn parse_host_just_host() {
        let (host, port) = parse_host("remote-server");
        assert_eq!(host, "remote-server");
        assert_eq!(port, None);
    }

    #[test]
    fn parse_host_host_with_port_no_user() {
        let (host, port) = parse_host("remote-server:2222");
        assert_eq!(host, "remote-server");
        assert_eq!(port, Some(2222));
    }

    #[test]
    fn parse_host_invalid_port_defaults_to_none() {
        // Invalid port number — should return None for port
        let (host, port) = parse_host("user@host:notanumber");
        // Invalid port defaults to 22
        assert_eq!(host, "user@host");
        assert_eq!(port, Some(22));
    }

    #[test]
    fn shell_escape_simple() {
        assert_eq!(shell_escape("hello"), "'hello'");
    }

    #[test]
    fn shell_escape_with_single_quote() {
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn shell_escape_with_spaces() {
        assert_eq!(
            shell_escape("/home/user/my project"),
            "'/home/user/my project'"
        );
    }

    #[test]
    fn shell_escape_empty() {
        assert_eq!(shell_escape(""), "''");
    }

    #[test]
    fn base64_encode_empty() {
        assert_eq!(base64_encode(b""), "");
    }

    #[test]
    fn base64_encode_hello() {
        // "hello" in base64 is "aGVsbG8="
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
    }

    #[test]
    fn base64_encode_three_bytes() {
        // "abc" in base64 is "YWJj"
        assert_eq!(base64_encode(b"abc"), "YWJj");
    }

    #[test]
    fn base64_encode_binary() {
        let bytes = vec![0x00, 0xFF, 0xFE, 0x7F];
        let encoded = base64_encode(&bytes);
        assert!(!encoded.is_empty());
        // Decode check: should be valid base64
        assert!(encoded.len() % 4 == 0);
    }

    #[test]
    fn ssh_transport_is_send_sync() {
        // Compile-time check: SshTransport must implement Send + Sync
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SshTransport>();
    }

    #[test]
    fn ssh_transport_debug() {
        let t = SshTransport::new("user@host", "/remote/workspace");
        let debug = format!("{:?}", t);
        assert!(debug.contains("SshTransport"));
        assert!(debug.contains("user@host"));
    }

    #[test]
    fn ssh_transport_clone() {
        let t = SshTransport::new("user@host", "/remote/workspace").with_key("/path/to/key");
        let t2 = t.clone();
        assert_eq!(t.host(), t2.host());
        assert_eq!(t.key_path(), t2.key_path());
        assert_eq!(t.remote_workspace(), t2.remote_workspace());
    }

    #[test]
    fn ssh_transport_implements_tool_transport() {
        // Compile-time check: SshTransport must implement ToolTransport
        fn assert_tool_transport<T: ToolTransport>() {}
        assert_tool_transport::<SshTransport>();
    }

    #[test]
    fn local_transport_implements_tool_transport() {
        fn assert_tool_transport<T: ToolTransport>() {}
        assert_tool_transport::<LocalTransport>();
    }

    /// Test that the SSH command construction includes ConnectTimeout.
    #[test]
    fn ssh_build_command_connect_timeout() {
        let t = SshTransport::new("user@host", "/remote/workspace")
            .with_connect_timeout(Duration::from_secs(15));
        let cmd = t.build_ssh_command("echo test");
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();

        assert!(args.contains(&"ConnectTimeout=15".to_string()));
    }

    /// Test that the default SSH command does NOT discard known_hosts
    /// verification (previously it appended UserKnownHostsFile=/dev/null).
    #[test]
    fn ssh_build_command_known_hosts_secure_by_default() {
        let t = SshTransport::new("user@host", "/remote/workspace");
        let cmd = t.build_ssh_command("echo test");
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();

        assert!(!args.contains(&"UserKnownHostsFile=/dev/null".to_string()));
    }

    /// The default SSH command must keep host key verification enabled.
    #[test]
    fn default_ssh_command_does_not_disable_host_key_checking() {
        let t = SshTransport::new("user@host", "/remote/workspace");
        let cmd = t.build_ssh_command("echo test");
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();

        assert!(!args.contains(&"StrictHostKeyChecking=no".to_string()));
        assert!(!args.contains(&"UserKnownHostsFile=/dev/null".to_string()));
        assert!(args.contains(&"BatchMode=yes".to_string()));
    }

    /// Opting in to insecure host key checking relaxes verification via
    /// `StrictHostKeyChecking=accept-new` (first-use trust).
    #[test]
    fn insecure_opt_in_enables_host_key_checking_bypass() {
        let t = SshTransport::new("user@host", "/remote/workspace")
            .with_insecure_host_key_checking(true);
        let cmd = t.build_ssh_command("echo test");
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();

        assert!(args.contains(&"StrictHostKeyChecking=accept-new".to_string()));
        assert!(!args.contains(&"StrictHostKeyChecking=no".to_string()));
        assert!(!args.contains(&"UserKnownHostsFile=/dev/null".to_string()));
    }

    /// A fresh transport must default to secure host key checking.
    #[test]
    fn insecure_opt_in_defaults_to_false() {
        let t = SshTransport::new("user@host", "/remote/workspace");
        assert!(!t.insecure_host_key_checking);
        let t = t.with_insecure_host_key_checking(true);
        assert!(t.insecure_host_key_checking);
        let t = t.with_insecure_host_key_checking(false);
        assert!(!t.insecure_host_key_checking);
    }
}
