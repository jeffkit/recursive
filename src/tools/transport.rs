//! Transport abstraction: decouple tools from direct filesystem/shell access.
//!
//! The `ToolTransport` trait lets tools delegate I/O to a pluggable backend.
//! The default `LocalTransport` calls `tokio::fs` / `tokio::process` directly.
//! A mock transport can be injected in tests to avoid touching the real disk.

use async_trait::async_trait;
use std::path::Path;
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
}
