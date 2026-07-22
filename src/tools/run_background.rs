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
use tokio::sync::Notify;
use tokio::time::timeout;

use super::resolve_within;
use super::Tool;
use crate::error::{Error, Result};
use crate::llm::ToolSpec;

/// Maximum bytes of stdout/stderr to capture per job.
const MAX_OUTPUT_BYTES: usize = 128 * 1024;

/// Maximum bytes of a watched file to surface in a single event-watch wake.
/// Keeps the injected prompt bounded; remaining bytes wake on the next poll.
pub(crate) const WATCH_CHUNK_BYTES: usize = 16 * 1024;

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

/// A file registered for mid-run event wakes. The TUI loop arbiter polls the
/// file and wakes the agent when bytes are appended past `offset`.
#[derive(Debug, Clone)]
pub struct WatchTarget {
    pub path: PathBuf,
    pub offset: u64,
}

/// Shared manager for background jobs.
pub struct BackgroundJobManager {
    jobs: HashMap<String, Job>,
    next_id: u64,
    /// Notifies waiters when a job enters a terminal state
    /// (Completed, Failed, TimedOut). Used by the TUI loop arbiter
    /// to wake on background job completion.
    completed_notify: Arc<Notify>,
    /// Optional file registered via the `watch_file` tool for mid-run
    /// event wakes. The loop arbiter polls it and wakes the agent when
    /// new bytes appear past `offset`.
    watch: Option<WatchTarget>,
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
            completed_notify: Arc::new(Notify::new()),
            watch: None,
        }
    }

    /// Return a clone of the completed-notify handle.
    ///
    /// Used by the TUI loop arbiter to `select!` on job completion.
    /// When a background job enters a terminal state (Completed, Failed,
    /// TimedOut), `notify_one()` is called. `notify_one()` (rather than
    /// `notify_waiters()`) stores a permit so a completion that happens
    /// while the arbiter is mid-turn (not yet polling) is not lost — the
    /// next `notified()` poll consumes the stored permit.
    pub fn completed_notify(&self) -> Arc<Notify> {
        self.completed_notify.clone()
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
        let is_terminal = matches!(
            state,
            JobState::Completed { .. } | JobState::Failed { .. } | JobState::TimedOut
        );
        if let Some(job) = self.jobs.get_mut(id) {
            job.state = state;
        }
        if is_terminal {
            self.completed_notify.notify_one();
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

    /// Register a file for mid-run event wakes. `offset` is the byte offset to
    /// start reading from (typically the file size at registration time, so
    /// only bytes appended afterwards wake the agent). Replaces any prior
    /// watch.
    pub fn set_watch(&mut self, path: PathBuf, offset: u64) {
        self.watch = Some(WatchTarget { path, offset });
    }

    /// Clear any registered file watch.
    pub fn clear_watch(&mut self) {
        self.watch = None;
    }

    /// Poll the registered watch file for new bytes past the stored offset.
    /// On success, advances the offset and returns the new content (capped at
    /// `WATCH_CHUNK_BYTES` per call; remaining bytes surface on the next
    /// poll). Returns `None` when no watch is set, the file is gone, or no new
    /// bytes are available. Handles log rotation (file shrank) by resetting to
    /// the start.
    pub fn poll_watch(&mut self) -> Option<String> {
        use std::io::{Read, Seek, SeekFrom};
        let watch = self.watch.as_ref()?;
        let path = watch.path.clone();
        let metadata = std::fs::metadata(&path).ok()?;
        let len = metadata.len();
        let mut offset = watch.offset;
        // Log rotation / truncation: reset to start.
        if len < offset {
            offset = 0;
        }
        if len <= offset {
            return None;
        }
        let mut file = std::fs::File::open(&path).ok()?;
        file.seek(SeekFrom::Start(offset)).ok()?;
        let to_read = ((len - offset) as usize).min(WATCH_CHUNK_BYTES);
        let mut buf = vec![0u8; to_read];
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        let new_offset = offset + n as u64;
        let text = String::from_utf8_lossy(&buf[..n]).into_owned();
        if let Some(w) = self.watch.as_mut() {
            w.offset = new_offset;
        }
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
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

    fn is_deferred(&self) -> bool {
        true
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
            call_id: None,
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

    fn is_deferred(&self) -> bool {
        true
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
#[cfg(not(target_os = "windows"))]
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

    #[tokio::test]
    async fn completed_notify_wakes_on_terminal_state() {
        let manager = Arc::new(Mutex::new(BackgroundJobManager::new()));

        let notify = manager.lock().await.completed_notify();

        // notify_one() stores a permit, so a waiter registered AFTER the
        // state change still wakes — this models the arbiter polling
        // between turns after a job completed mid-turn (the real usage).
        {
            let mut mgr = manager.lock().await;
            let id = mgr.insert(Job {
                state: JobState::Running,
                created_at: Instant::now(),
            });
            mgr.update(
                &id,
                JobState::Completed {
                    stdout: "done".into(),
                    stderr: "".into(),
                    exit_code: 0,
                },
            );
        }

        let result = tokio::time::timeout(Duration::from_secs(5), notify.notified()).await;
        assert!(result.is_ok(), "notify should fire on terminal state");
    }

    #[tokio::test]
    async fn completed_notify_does_not_fire_for_running_job() {
        let manager = Arc::new(Mutex::new(BackgroundJobManager::new()));

        let notify = manager.lock().await.completed_notify();

        // Insert a running job (should NOT trigger notify).
        {
            let mut mgr = manager.lock().await;
            mgr.insert(Job {
                state: JobState::Running,
                created_at: Instant::now(),
            });
        }

        // The notify should timeout (running jobs don't fire it).
        let result = tokio::time::timeout(Duration::from_millis(200), notify.notified()).await;
        assert!(result.is_err(), "notify should not fire for running jobs");
    }

    #[tokio::test]
    async fn completed_notify_fires_for_timed_out_job() {
        let manager = Arc::new(Mutex::new(BackgroundJobManager::new()));

        let notify = manager.lock().await.completed_notify();

        let handle = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(5), notify.notified()).await
        });

        {
            let mut mgr = manager.lock().await;
            let id = mgr.insert(Job {
                state: JobState::Running,
                created_at: Instant::now(),
            });
            mgr.update(&id, JobState::TimedOut);
        }

        let result = handle.await.unwrap();
        assert!(result.is_ok(), "notify should fire for TimedOut");
    }

    #[tokio::test]
    async fn completed_notify_fires_for_failed_job() {
        let manager = Arc::new(Mutex::new(BackgroundJobManager::new()));

        let notify = manager.lock().await.completed_notify();

        let handle = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(5), notify.notified()).await
        });

        {
            let mut mgr = manager.lock().await;
            let id = mgr.insert(Job {
                state: JobState::Running,
                created_at: Instant::now(),
            });
            mgr.update(
                &id,
                JobState::Failed {
                    message: "boom".into(),
                },
            );
        }

        let result = handle.await.unwrap();
        assert!(result.is_ok(), "notify should fire for Failed");
    }

    #[tokio::test]
    async fn shared_bg_manager_sees_jobs_from_tool() {
        let tmp = TempDir::new().unwrap();
        let manager = Arc::new(Mutex::new(BackgroundJobManager::new()));

        let run_tool = RunBackground::new(tmp.path(), manager.clone());

        // Run a quick command
        let result = run_tool
            .execute(json!({"command": "echo shared-test"}))
            .await
            .unwrap();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        let job_id = parsed["job_id"].as_str().unwrap().to_string();
        assert_eq!(parsed["status"], "spawned");

        // Wait for completion via the shared manager.
        let notify = manager.lock().await.completed_notify();
        tokio::time::timeout(Duration::from_secs(5), notify.notified())
            .await
            .expect("notify should fire");

        // take_completed from the shared manager should return the job.
        let mut mgr = manager.lock().await;
        let (id, output) = mgr.take_completed().unwrap();
        assert_eq!(id, job_id);
        assert!(output.contains("shared-test"));
    }

    // ── BackgroundJobManager unit tests ─────────────────────────────────────

    #[tokio::test]
    async fn manager_next_id_increments() {
        let mut mgr = BackgroundJobManager::new();
        let id0 = mgr.next_id();
        let id1 = mgr.next_id();
        let id2 = mgr.next_id();
        // next_id starts at 1
        assert_eq!(id0, "bg-1");
        assert_eq!(id1, "bg-2");
        assert_eq!(id2, "bg-3");
    }

    #[tokio::test]
    async fn manager_insert_returns_id_and_stores_job() {
        let mut mgr = BackgroundJobManager::new();
        let job = Job {
            state: JobState::Running,
            created_at: Instant::now(),
        };
        let id = mgr.insert(job);
        assert_eq!(id, "bg-1");
        assert!(mgr.get_state("bg-1").is_some());
        assert!(matches!(mgr.get_state("bg-1"), Some(JobState::Running)));
    }

    #[tokio::test]
    async fn manager_get_state_returns_none_for_unknown() {
        let mgr = BackgroundJobManager::new();
        assert!(mgr.get_state("bg-99").is_none());
    }

    #[tokio::test]
    async fn manager_update_changes_state() {
        let mut mgr = BackgroundJobManager::new();
        let job = Job {
            state: JobState::Running,
            created_at: Instant::now(),
        };
        let id = mgr.insert(job);
        mgr.update(
            &id,
            JobState::Completed {
                stdout: "out".into(),
                stderr: "".into(),
                exit_code: 0,
            },
        );
        assert!(matches!(
            mgr.get_state(&id),
            Some(JobState::Completed { exit_code: 0, .. })
        ));
    }

    #[tokio::test]
    async fn manager_clear_removes_all_jobs() {
        let mut mgr = BackgroundJobManager::new();
        let id0 = mgr.insert(Job {
            state: JobState::Running,
            created_at: Instant::now(),
        });
        let id1 = mgr.insert(Job {
            state: JobState::Running,
            created_at: Instant::now(),
        });
        assert!(mgr.get_state(&id0).is_some());
        mgr.clear();
        assert!(mgr.get_state(&id0).is_none());
        assert!(mgr.get_state(&id1).is_none());
    }

    #[tokio::test]
    async fn manager_take_completed_running_job_returns_none() {
        let mut mgr = BackgroundJobManager::new();
        mgr.insert(Job {
            state: JobState::Running,
            created_at: Instant::now(),
        });
        assert!(
            mgr.take_completed().is_none(),
            "running job must not be taken"
        );
    }

    #[tokio::test]
    async fn manager_take_completed_removes_and_returns_job() {
        let mut mgr = BackgroundJobManager::new();
        let job = Job {
            state: JobState::Completed {
                stdout: "hello".into(),
                stderr: "".into(),
                exit_code: 0,
            },
            created_at: Instant::now(),
        };
        let id = mgr.insert(job);

        let (taken_id, output) = mgr.take_completed().expect("must take completed job");
        assert_eq!(taken_id, id);
        assert!(output.contains("hello"), "output must contain stdout");
        assert!(
            output.contains("exit code: 0"),
            "output must contain exit code"
        );

        // After take, job is gone
        assert!(mgr.get_state(&id).is_none());
    }

    #[tokio::test]
    async fn manager_take_completed_failed_job() {
        let mut mgr = BackgroundJobManager::new();
        let job = Job {
            state: JobState::Failed {
                message: "oops".into(),
            },
            created_at: Instant::now(),
        };
        let id = mgr.insert(job);
        let (_, output) = mgr.take_completed().expect("must take failed job");
        assert!(
            output.contains("oops"),
            "output must contain failure message"
        );
        assert!(mgr.get_state(&id).is_none());
    }

    #[tokio::test]
    async fn manager_cleanup_removes_old_jobs() {
        let mut mgr = BackgroundJobManager::new();
        // Simulate a very old job by using an instant far in the past.
        let old_created = Instant::now() - JOB_TTL - Duration::from_secs(1);
        let old_job = Job {
            state: JobState::Completed {
                stdout: "".into(),
                stderr: "".into(),
                exit_code: 0,
            },
            created_at: old_created,
        };
        let old_id = mgr.insert(old_job);

        // A fresh running job should survive cleanup.
        let fresh_job = Job {
            state: JobState::Running,
            created_at: Instant::now(),
        };
        let fresh_id = mgr.insert(fresh_job);

        mgr.cleanup();

        assert!(
            mgr.get_state(&old_id).is_none(),
            "old job must be removed by cleanup"
        );
        assert!(
            mgr.get_state(&fresh_id).is_some(),
            "fresh job must survive cleanup"
        );
    }
}
