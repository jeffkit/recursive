//! `run_background` and `check_background`: spawn long-running commands
//! asynchronously and poll their status later.
//!
//! `run_background` returns a job ID immediately so the agent can continue
//! working while the command runs. `check_background` retrieves the output
//! (or partial output) of a previously spawned job.
//!
//! Jobs are stored in a shared `BackgroundJobManager` and cleaned up after
//! a configurable TTL or when explicitly checked.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::timeout;

use super::resolve_within;
use super::Tool;
use crate::error::{Error, Result};
use crate::llm::ToolSpec;

/// Maximum bytes of stdout/stderr to capture per job.
const MAX_OUTPUT_BYTES: usize = 128 * 1024;

/// Default timeout for a background job (if none specified).
const DEFAULT_JOB_TIMEOUT: u64 = 3600; // 1 hour

/// How long a completed/failed job's output is kept before cleanup.
const JOB_TTL: Duration = Duration::from_secs(3600);

/// State of a background job.
#[derive(Debug, Clone)]
pub enum JobState {
    Running,
    Completed {
        stdout: String,
        stderr: String,
        exit_code: i32,
    },
    Failed {
        message: String,
    },
    TimedOut,
}

/// A single background job.
pub struct Job {
    pub state: JobState,
    pub created_at: Instant,
}

/// Shared manager for background jobs.
pub struct BackgroundJobManager {
    jobs: HashMap<String, Job>,
    next_id: u64,
}

impl Default for BackgroundJobManager {
    fn default() -> Self {
        Self::new()
    }
}

impl BackgroundJobManager {
    pub fn new() -> Self {
        Self {
            jobs: HashMap::new(),
            next_id: 1,
        }
    }

    /// Generate a new unique job ID.
    fn next_id(&mut self) -> String {
        let id = self.next_id;
        self.next_id += 1;
        format!("bg-{id}")
    }

    /// Insert a new job and return its ID.
    pub fn insert(&mut self, job: Job) -> String {
        let id = self.next_id();
        self.jobs.insert(id.clone(), job);
        id
    }

    /// Get a reference to a job's state.
    pub fn get_state(&self, id: &str) -> Option<JobState> {
        self.jobs.get(id).map(|j| j.state.clone())
    }

    /// Update a job's state.
    fn update(&mut self, id: &str, state: JobState) {
        if let Some(job) = self.jobs.get_mut(id) {
            job.state = state;
        }
    }

    /// Remove stale jobs (older than TTL).
    fn cleanup(&mut self) {
        let cutoff = Instant::now() - JOB_TTL;
        self.jobs.retain(|_, job| job.created_at > cutoff);
    }

    /// Remove all jobs immediately.
    pub fn clear(&mut self) {
        self.jobs.clear();
    }

    /// Remove and return the first completed job, if any.
    ///
    /// Returns `Some((job_id, output_string))` where `output_string` includes
    /// the exit code and captured stdout/stderr. Completed jobs are removed
    /// from the tracked set.
    pub fn take_completed(&mut self) -> Option<(String, String)> {
        let id = self.jobs.iter().find_map(|(id, job)| match &job.state {
            JobState::Completed { .. } | JobState::Failed { .. } | JobState::TimedOut => {
                Some(id.clone())
            }
            JobState::Running => None,
        })?;
        let job = self.jobs.remove(&id)?;
        let output = match &job.state {
            JobState::Completed {
                stdout,
                stderr,
                exit_code,
            } => {
                format!("exit code: {exit_code}\nstdout:\n{stdout}\nstderr:\n{stderr}")
            }
            JobState::Failed { message } => {
                format!("failed: {message}")
            }
            JobState::TimedOut => "timed out".to_string(),
            JobState::Running => unreachable!(), // filtered above
        };
        Some((id, output))
    }
}

/// The `run_background` tool: spawn a command and return immediately.
pub struct RunBackground {
    root: PathBuf,
    manager: Arc<Mutex<BackgroundJobManager>>,
}

impl RunBackground {
    pub fn new(root: impl Into<PathBuf>, manager: Arc<Mutex<BackgroundJobManager>>) -> Self {
        Self {
            root: root.into(),
            manager,
        }
    }
}

#[async_trait]
impl Tool for RunBackground {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_background".into(),
            description:
                "Run a shell command in the background and return immediately with a job ID. \
                 Use `check_background` later to retrieve the output. Useful for long-running \
                 commands (e.g. builds, tests, downloads) that you don't want to block on."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Command line to execute via sh -c"
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Optional subdirectory (relative to workspace root) to run the command in. Must stay inside the workspace."
                    },
                    "env": {
                        "type": "object",
                        "description": "Optional extra env vars set for this command only. Values must be strings; non-string values are rejected.",
                        "additionalProperties": {
                            "type": "string"
                        }
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Optional timeout in seconds (default 3600, max 86400)",
                        "default": 3600
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let command = args["command"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "run_background".into(),
            message: "missing `command`".into(),
        })?;

        let cwd = if let Some(rel) = args.get("cwd").and_then(|v| v.as_str()) {
            resolve_within(&self.root, rel).map_err(|e| Error::BadToolArgs {
                name: "run_background".into(),
                message: format!("cwd: {e}"),
            })?
        } else {
            self.root.clone()
        };

        let timeout_secs = args
            .get("timeout_secs")
            .and_then(|v| v.as_i64())
            .unwrap_or(DEFAULT_JOB_TIMEOUT as i64)
            .clamp(1, 86400) as u64;

        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(command);
        cmd.current_dir(&cwd);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        // Apply optional env overrides
        if let Some(env_map) = args.get("env").and_then(|v| v.as_object()) {
            for (key, val) in env_map {
                let val_str = val.as_str().ok_or_else(|| Error::BadToolArgs {
                    name: "run_background".to_string(),
                    message: format!("env value for `{key}` must be a string, got {:?}", val),
                })?;
                cmd.env(key, val_str);
            }
        }

        // Spawn the process
        let mut child = cmd.spawn().map_err(|e| Error::Tool {
            name: "run_background".into(),
            message: format!("spawn failed: {e}"),
        })?;

        let stdout_handle = child.stdout.take();
        let stderr_handle = child.stderr.take();

        // Generate a job ID and store it
        let mut manager = self.manager.lock().await;
        manager.cleanup();
        let job_id = manager.insert(Job {
            state: JobState::Running,
            created_at: Instant::now(),
        });
        drop(manager);

        // Spawn a background task to wait for the process and capture output
        let manager_clone = self.manager.clone();
        let job_id_clone = job_id.clone();
        tokio::spawn(async move {
            let result = run_background_job(
                child,
                stdout_handle,
                stderr_handle,
                Duration::from_secs(timeout_secs),
            )
            .await;

            let mut mgr = manager_clone.lock().await;
            mgr.update(&job_id_clone, result);
        });

        Ok(json!({
            "job_id": job_id,
            "status": "spawned",
            "message": format!("Background job `{}` spawned. Use `check_background` with job_id `{}` to retrieve output.", command, job_id)
        })
        .to_string())
    }
}

/// The `check_background` tool: poll a previously spawned background job.
pub struct CheckBackground {
    manager: Arc<Mutex<BackgroundJobManager>>,
}

impl CheckBackground {
    pub fn new(manager: Arc<Mutex<BackgroundJobManager>>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for CheckBackground {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "check_background".into(),
            description: "Check the status and output of a previously spawned background job. \
                 Returns the current state (running, completed, failed, or timed out) \
                 along with any captured stdout/stderr."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "job_id": {
                        "type": "string",
                        "description": "The job ID returned by `run_background`"
                    }
                },
                "required": ["job_id"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let job_id = args["job_id"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "check_background".into(),
            message: "missing `job_id`".into(),
        })?;

        let mut manager = self.manager.lock().await;
        manager.cleanup();

        match manager.get_state(job_id) {
            None => Ok(json!({
                "job_id": job_id,
                "status": "unknown",
                "message": format!("No job found with ID `{}`. It may have expired or the ID is invalid.", job_id)
            })
            .to_string()),
            Some(JobState::Running) => Ok(json!({
                "job_id": job_id,
                "status": "running",
                "message": "Job is still running. Check again later."
            })
            .to_string()),
            Some(JobState::Completed { stdout, stderr, exit_code }) => {
                // Clean up completed job after returning its output
                manager.update(job_id, JobState::Completed {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code,
                });
                Ok(json!({
                    "job_id": job_id,
                    "status": "completed",
                    "exit_code": exit_code,
                    "stdout": stdout,
                    "stderr": stderr
                })
                .to_string())
            }
            Some(JobState::Failed { message }) => {
                Ok(json!({
                    "job_id": job_id,
                    "status": "failed",
                    "message": message
                })
                .to_string())
            }
            Some(JobState::TimedOut) => Ok(json!({
                "job_id": job_id,
                "status": "timed_out",
                "message": "Job exceeded its timeout."
            })
            .to_string()),
        }
    }
}

/// Wait for a background process to finish, capturing its output.
async fn run_background_job(
    mut child: tokio::process::Child,
    stdout_opt: Option<tokio::process::ChildStdout>,
    stderr_opt: Option<tokio::process::ChildStderr>,
    job_timeout: Duration,
) -> JobState {
    let stdout_task = async {
        let mut out = String::new();
        if let Some(mut reader) = stdout_opt {
            let mut buf = [0u8; 8192];
            let mut total = 0usize;
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if total + n > MAX_OUTPUT_BYTES {
                            let take = MAX_OUTPUT_BYTES.saturating_sub(total);
                            out.push_str(&String::from_utf8_lossy(&buf[..take]));
                            out.push_str("\n... [stdout truncated]");
                            // Drain remaining
                            let _ = tokio::io::copy(&mut reader, &mut tokio::io::sink()).await;
                            break;
                        }
                        out.push_str(&String::from_utf8_lossy(&buf[..n]));
                        total += n;
                    }
                    Err(_) => break,
                }
            }
        }
        out
    };

    let stderr_task = async {
        let mut err = String::new();
        if let Some(mut reader) = stderr_opt {
            let mut buf = [0u8; 8192];
            let mut total = 0usize;
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if total + n > MAX_OUTPUT_BYTES {
                            let take = MAX_OUTPUT_BYTES.saturating_sub(total);
                            err.push_str(&String::from_utf8_lossy(&buf[..take]));
                            err.push_str("\n... [stderr truncated]");
                            let _ = tokio::io::copy(&mut reader, &mut tokio::io::sink()).await;
                            break;
                        }
                        err.push_str(&String::from_utf8_lossy(&buf[..n]));
                        total += n;
                    }
                    Err(_) => break,
                }
            }
        }
        err
    };

    // Wait for the process with a timeout
    let wait_result = timeout(job_timeout, child.wait()).await;

    match wait_result {
        Err(_) => {
            // Timed out — kill the process
            let _ = child.start_kill();
            JobState::TimedOut
        }
        Ok(Err(e)) => JobState::Failed {
            message: format!("wait failed: {e}"),
        },
        Ok(Ok(status)) => {
            let stdout = stdout_task.await;
            let stderr = stderr_task.await;
            let exit_code = status.code().unwrap_or(-1);
            JobState::Completed {
                stdout,
                stderr,
                exit_code,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;
    use tokio::time::sleep;

    /// Poll `check_background` until the job is no longer running, with a timeout.
    async fn poll_until_done(
        check_tool: &CheckBackground,
        job_id: &str,
        max_wait: Duration,
    ) -> Value {
        let start = Instant::now();
        loop {
            let result = check_tool.execute(json!({"job_id": job_id})).await.unwrap();
            let parsed: Value = serde_json::from_str(&result).unwrap();
            if parsed["status"] != "running" {
                return parsed;
            }
            if start.elapsed() > max_wait {
                panic!("timed out waiting for job {job_id} to complete");
            }
            sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn run_and_check_background() {
        let tmp = TempDir::new().unwrap();
        let manager = Arc::new(Mutex::new(BackgroundJobManager::new()));

        let run_tool = RunBackground::new(tmp.path(), manager.clone());
        let check_tool = CheckBackground::new(manager.clone());

        // Run a quick command
        let result = run_tool
            .execute(json!({"command": "echo hello world"}))
            .await
            .unwrap();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        let job_id = parsed["job_id"].as_str().unwrap().to_string();
        assert_eq!(parsed["status"], "spawned");

        let parsed = poll_until_done(&check_tool, &job_id, Duration::from_secs(5)).await;
        assert_eq!(parsed["status"], "completed");
        assert_eq!(parsed["exit_code"], 0);
        assert!(parsed["stdout"].as_str().unwrap().contains("hello world"));
    }

    #[tokio::test]
    async fn background_job_timeout() {
        let tmp = TempDir::new().unwrap();
        let manager = Arc::new(Mutex::new(BackgroundJobManager::new()));

        let run_tool = RunBackground::new(tmp.path(), manager.clone());
        let check_tool = CheckBackground::new(manager.clone());

        // Run a command that sleeps longer than the timeout
        let result = run_tool
            .execute(json!({"command": "sleep 10", "timeout_secs": 1}))
            .await
            .unwrap();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        let job_id = parsed["job_id"].as_str().unwrap().to_string();

        let parsed = poll_until_done(&check_tool, &job_id, Duration::from_secs(5)).await;
        assert_eq!(parsed["status"], "timed_out");
    }

    #[tokio::test]
    async fn background_job_nonzero_exit() {
        let tmp = TempDir::new().unwrap();
        let manager = Arc::new(Mutex::new(BackgroundJobManager::new()));

        let run_tool = RunBackground::new(tmp.path(), manager.clone());
        let check_tool = CheckBackground::new(manager.clone());

        let result = run_tool
            .execute(json!({"command": "exit 42"}))
            .await
            .unwrap();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        let job_id = parsed["job_id"].as_str().unwrap().to_string();

        let parsed = poll_until_done(&check_tool, &job_id, Duration::from_secs(5)).await;
        assert_eq!(parsed["status"], "completed");
        assert_eq!(parsed["exit_code"], 42);
    }

    #[tokio::test]
    async fn check_unknown_job() {
        let manager = Arc::new(Mutex::new(BackgroundJobManager::new()));
        let check_tool = CheckBackground::new(manager);

        let result = check_tool
            .execute(json!({"job_id": "bg-999"}))
            .await
            .unwrap();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["status"], "unknown");
    }

    #[tokio::test]
    async fn run_background_with_cwd() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("subdir");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("marker.txt"), "content").unwrap();

        let manager = Arc::new(Mutex::new(BackgroundJobManager::new()));
        let run_tool = RunBackground::new(tmp.path(), manager.clone());
        let check_tool = CheckBackground::new(manager.clone());

        let result = run_tool
            .execute(json!({"command": "ls", "cwd": "subdir"}))
            .await
            .unwrap();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        let job_id = parsed["job_id"].as_str().unwrap().to_string();

        let parsed = poll_until_done(&check_tool, &job_id, Duration::from_secs(5)).await;
        assert_eq!(parsed["status"], "completed");
        assert!(parsed["stdout"].as_str().unwrap().contains("marker.txt"));
    }

    #[tokio::test]
    async fn run_background_with_env() {
        let tmp = TempDir::new().unwrap();
        let manager = Arc::new(Mutex::new(BackgroundJobManager::new()));
        let run_tool = RunBackground::new(tmp.path(), manager.clone());
        let check_tool = CheckBackground::new(manager.clone());

        let result = run_tool
            .execute(json!({"command": "echo $MY_VAR", "env": {"MY_VAR": "hello"}}))
            .await
            .unwrap();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        let job_id = parsed["job_id"].as_str().unwrap().to_string();

        let parsed = poll_until_done(&check_tool, &job_id, Duration::from_secs(5)).await;
        assert_eq!(parsed["status"], "completed");
        assert!(parsed["stdout"].as_str().unwrap().contains("hello"));
    }

    #[tokio::test]
    async fn run_background_missing_command() {
        let tmp = TempDir::new().unwrap();
        let manager = Arc::new(Mutex::new(BackgroundJobManager::new()));
        let run_tool = RunBackground::new(tmp.path(), manager);

        let err = run_tool.execute(json!({})).await.unwrap_err();
        assert!(matches!(err, Error::BadToolArgs { .. }));
    }

    #[tokio::test]
    async fn check_background_missing_job_id() {
        let manager = Arc::new(Mutex::new(BackgroundJobManager::new()));
        let check_tool = CheckBackground::new(manager);

        let err = check_tool.execute(json!({})).await.unwrap_err();
        assert!(matches!(err, Error::BadToolArgs { .. }));
    }

    #[tokio::test]
    async fn take_completed_returns_finished_job() {
        let mut manager = BackgroundJobManager::new();
        // Insert a running job
        let running_id = manager.insert(Job {
            state: JobState::Running,
            created_at: Instant::now(),
        });
        // Insert a completed job
        let completed_id = manager.insert(Job {
            state: JobState::Completed {
                stdout: "hello".into(),
                stderr: "".into(),
                exit_code: 0,
            },
            created_at: Instant::now(),
        });

        // take_completed should return the completed job
        let (id, output) = manager.take_completed().unwrap();
        assert_eq!(id, completed_id);
        assert!(output.contains("hello"));
        assert!(output.contains("exit code: 0"));

        // Running job should still be there
        assert!(manager.get_state(&running_id).is_some());
        // Completed job should be removed
        assert!(manager.get_state(&completed_id).is_none());

        // No more completed jobs
        assert!(manager.take_completed().is_none());
    }

    #[tokio::test]
    async fn take_completed_returns_failed_job() {
        let mut manager = BackgroundJobManager::new();
        manager.insert(Job {
            state: JobState::Failed {
                message: "something went wrong".into(),
            },
            created_at: Instant::now(),
        });

        let (id, output) = manager.take_completed().unwrap();
        assert!(id.starts_with("bg-"));
        assert!(output.contains("failed"));
        assert!(output.contains("something went wrong"));
    }

    #[tokio::test]
    async fn take_completed_returns_timed_out_job() {
        let mut manager = BackgroundJobManager::new();
        manager.insert(Job {
            state: JobState::TimedOut,
            created_at: Instant::now(),
        });

        let (id, output) = manager.take_completed().unwrap();
        assert!(id.starts_with("bg-"));
        assert!(output.contains("timed out"));
    }

    #[tokio::test]
    async fn take_completed_empty_when_no_finished_jobs() {
        let mut manager = BackgroundJobManager::new();
        manager.insert(Job {
            state: JobState::Running,
            created_at: Instant::now(),
        });
        assert!(manager.take_completed().is_none());
    }
}
