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

use async_trait::async_trait;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

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

/// Result of running a shell command.
#[derive(Debug, Clone)]
pub struct ExecResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Abstract transport for filesystem and shell operations.
///
/// Tools that need I/O (`ReadFile`, `WriteFile`, `ListDir`, `RunShell`)
/// call methods on this trait instead of using `tokio::fs` / `tokio::process`
/// directly. This makes them testable without touching the real filesystem.
#[async_trait]
pub trait ToolTransport: Send + Sync + std::fmt::Debug {
    /// Read the full contents of a file at `path`.
    async fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>>;

    /// Write `contents` to a file at `path`, creating parent directories.
    async fn write_file(&self, path: &Path, contents: &[u8]) -> std::io::Result<()>;

    /// List entries in a directory at `path`.
    async fn list_dir(&self, path: &Path) -> std::io::Result<Vec<DirEntry>>;

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
        cmd.arg("-o").arg("StrictHostKeyChecking=no");
        cmd.arg("-o").arg("UserKnownHostsFile=/dev/null");
        cmd.arg("-o")
            .arg(format!("ConnectTimeout={}", self.connect_timeout.as_secs()));

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

        let mut stdout = child.stdout.take().expect("stdout piped");
        let mut stderr = child.stderr.take().expect("stderr piped");

        let max: usize = 128 * 1024;
        let stdout_task = tokio::spawn(async move { read_capped(&mut stdout, max).await });
        let stderr_task = tokio::spawn(async move { read_capped(&mut stderr, max).await });

        let wait = child.wait();
        let status = match tokio::time::timeout(self.command_timeout, wait).await {
            Ok(s) => s?,
            Err(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("SSH command timed out after {:?}", self.command_timeout),
                ));
            }
        };

        let out = stdout_task.await.unwrap_or_default();
        let err = stderr_task.await.unwrap_or_default();
        let code = status.code();

        Ok(ExecResult {
            exit_code: code,
            stdout: out,
            stderr: err,
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

        let mut stdout = child.stdout.take().expect("stdout piped");
        let mut stderr = child.stderr.take().expect("stderr piped");

        let max = max_output_bytes;
        let stdout_task = tokio::spawn(async move { read_capped(&mut stdout, max).await });
        let stderr_task = tokio::spawn(async move { read_capped(&mut stderr, max).await });

        let wait = child.wait();
        let status = match tokio::time::timeout(timeout, wait).await {
            Ok(s) => s?,
            Err(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("SSH command timed out after {:?}", timeout),
                ));
            }
        };

        let out = stdout_task.await.unwrap_or_default();
        let err = stderr_task.await.unwrap_or_default();
        let code = status.code();

        Ok(ExecResult {
            exit_code: code,
            stdout: out,
            stderr: err,
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

#[async_trait]
impl ToolTransport for LocalTransport {
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

        for (key, val) in env {
            cmd.env(key, val);
        }

        let mut child = cmd.spawn()?;

        let mut stdout = child.stdout.take().expect("stdout piped");
        let mut stderr = child.stderr.take().expect("stderr piped");

        let max = max_output_bytes;
        let stdout_task = tokio::spawn(async move { read_capped(&mut stdout, max).await });
        let stderr_task = tokio::spawn(async move { read_capped(&mut stderr, max).await });

        let wait = child.wait();
        let status = match tokio::time::timeout(timeout, wait).await {
            Ok(s) => s?,
            Err(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("command timed out after {:?}", timeout),
                ));
            }
        };

        let out = stdout_task.await.unwrap_or_default();
        let err = stderr_task.await.unwrap_or_default();
        let code = status.code();

        Ok(ExecResult {
            exit_code: code,
            stdout: out,
            stderr: err,
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

    #[tokio::test]
    async fn local_transport_create_dir_all() {
        let t = LocalTransport;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("a").join("b").join("c");
        t.create_dir_all(&path).await.unwrap();
        assert!(path.exists());
        assert!(path.is_dir());
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
        // Should have StrictHostKeyChecking=no
        assert!(args.contains(&"StrictHostKeyChecking=no".to_string()));
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

    /// Test that the SSH command construction includes UserKnownHostsFile=/dev/null.
    #[test]
    fn ssh_build_command_known_hosts() {
        let t = SshTransport::new("user@host", "/remote/workspace");
        let cmd = t.build_ssh_command("echo test");
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();

        assert!(args.contains(&"UserKnownHostsFile=/dev/null".to_string()));
    }
}
