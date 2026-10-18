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

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::process::{CancelHandle, CancelKind, CancelReason, TaskRegistration};
use crate::tools::{resolve_within, SessionToolState, Tool};

/// Maximum bytes of stdout/stderr to capture per job.
const MAX_OUTPUT_BYTES: usize = 128 * 1024;

/// How long destroy-time liquidation waits for each cancelled job to report
/// that it reaped its process group before calling it *unreclaimed*.
pub const DEFAULT_RECLAIM_GRACE: Duration = Duration::from_secs(2);

/// Bound on how long the stdout/stderr readers may keep the job waiting for
/// EOF once the process group is gone. A descendant that escaped the group
/// (`setsid`) can hold the write end open forever, so the drain is bounded
/// exactly like the transport's.
const READER_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Default grace period for subprocess graceful shutdown (SIGTERM → wait →
/// SIGKILL). Lives here (not in `mcp`) so `acp` can use it without depending
/// on the `mcp` feature — it is a plain duration constant, not MCP logic.
pub const DEFAULT_KILL_GRACE_PERIOD: std::time::Duration = std::time::Duration::from_secs(5);

/// Maximum bytes of a watched file to surface in a single event-watch wake.
/// Keeps the injected prompt bounded; remaining bytes wake on the next poll.
pub(crate) const WATCH_CHUNK_BYTES: usize = 16 * 1024;

/// Default timeout for a background job (if none specified).
const DEFAULT_JOB_TIMEOUT: u64 = 3600; // 1 hour

/// How long a completed/failed job's output is kept before cleanup.
const JOB_TTL: Duration = Duration::from_secs(3600);

/// Monotonic discriminator for job tables inside one process: a process can
/// host several sessions, each with its own table and therefore its own
/// cancellation scope.
fn next_scope_id() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    format!(
        "bgm-{}",
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

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
    /// Goal 412: a cancel handle was consumed and the job's process group was
    /// reaped. Distinct from [`JobState::TimedOut`] so the caller can tell an
    /// operator/destroy stop from a budget overrun.
    Cancelled,
}

impl JobState {
    /// True if the job will never change state again.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            JobState::Completed { .. }
                | JobState::Failed { .. }
                | JobState::TimedOut
                | JobState::Cancelled
        )
    }
}

/// A single background job.
pub struct Job {
    pub state: JobState,
    pub created_at: Instant,
}

impl Job {
    /// A freshly spawned, still-running job.
    pub fn running() -> Self {
        Self {
            state: JobState::Running,
            created_at: Instant::now(),
        }
    }
}

/// A file registered for mid-run event wakes. The TUI loop arbiter polls the
/// file and wakes the agent when bytes are appended past `offset`.
#[derive(Debug, Clone)]
pub struct WatchTarget {
    pub path: PathBuf,
    pub offset: u64,
}

/// Agent-requested loop control written by the `stop_loop` tool and read by
/// the TUI loop arbiter between turns. Lets the agent end the event-driven
/// loop itself (so the user can say "stop" in natural language and the agent
/// calls `stop_loop`), making the loop lifecycle transparent to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopControl {
    /// Stop the loop after the current turn and return to interactive mode.
    Stop,
}

/// Shared manager for background jobs.
pub struct BackgroundJobManager {
    jobs: HashMap<String, Job>,
    /// Goal 412: one cancel registration per live job, keyed by job id. While
    /// an entry lives the job is addressable — a cancel routed by key or scope
    /// (or destroy-time liquidation) reaches the job loop, which reaps the
    /// process group. Removing the entry (terminal state, TTL cleanup, table
    /// clear) deregisters it, so a finished job is never left routable.
    cancels: HashMap<String, TaskRegistration>,
    next_id: u64,
    /// Goal 412: this job table's own scope id. Every job it registers is
    /// addressable as `background-job:<scope>/<job-id>`, so cancellation can be
    /// routed per *execution* — one process may host several sessions.
    scope: String,
    /// Notifies waiters when a job enters a terminal state
    /// (Completed, Failed, TimedOut, Cancelled). Used by the TUI loop arbiter
    /// to wake on background job completion.
    completed_notify: Arc<Notify>,
    /// Optional file registered via the `watch_file` tool for mid-run
    /// event wakes. The loop arbiter polls it and wakes the agent when
    /// new bytes appear past `offset`.
    watch: Option<WatchTarget>,
    /// Agent-requested loop control (set by the `stop_loop` tool). The loop
    /// arbiter drains this between turns so the agent can end the loop itself.
    loop_control: Option<LoopControl>,
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
            cancels: HashMap::new(),
            next_id: 1,
            scope: next_scope_id(),
            completed_notify: Arc::new(Notify::new()),
            watch: None,
            loop_control: None,
        }
    }

    /// This job table's cancellation scope (opaque, stable for its lifetime).
    pub fn scope(&self) -> &str {
        &self.scope
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
    ///
    /// Goal 412: insertion registers a cancel handle for the job in the
    /// process-wide task table, keyed `background-job:<scope>/<job-id>` — every
    /// job is cancellable and routable from the moment it exists.
    pub fn insert(&mut self, job: Job) -> String {
        let id = self.next_id();
        let registration = TaskRegistration::spawn_keyed(
            CancelKind::BackgroundJob,
            Some(&self.scope),
            format!(
                "{}:{}/{}",
                CancelKind::BackgroundJob.as_str(),
                self.scope,
                id
            ),
        );
        registration.note_stage("spawned");
        self.cancels.insert(id.clone(), registration);
        self.jobs.insert(id.clone(), job);
        id
    }

    /// Clone the cancel handle of a tracked job, if it still has one.
    pub fn cancel_handle(&self, id: &str) -> Option<CancelHandle> {
        self.cancels
            .get(id)
            .map(|registration| registration.handle().clone())
    }

    /// Cancel one running job by ID. The job loop observes the cancel, reaps
    /// the process group and records [`JobState::Cancelled`].
    pub fn cancel(&mut self, id: &str) -> bool {
        match self.cancels.get(id) {
            Some(registration) if !self.is_terminal(id) => registration.cancel(),
            _ => false,
        }
    }

    /// Cancel every running job (destroy-time liquidation). Returns how many
    /// were newly cancelled. State is *not* mutated here: the job loop owns the
    /// transition, so a job that never reports back stays visibly Running and
    /// surfaces as unreclaimed (see [`reclaim_all`]).
    pub fn cancel_all(&mut self) -> usize {
        let live: Vec<String> = self
            .cancels
            .keys()
            .filter(|id| !self.is_terminal(id))
            .cloned()
            .collect();
        live.iter()
            .filter_map(|id| self.cancels.get(id))
            .filter(|registration| registration.cancel_with(CancelReason::Destroy))
            .count()
    }

    fn is_terminal(&self, id: &str) -> bool {
        self.jobs
            .get(id)
            .map(|job| job.state.is_terminal())
            .unwrap_or(true)
    }

    /// Number of jobs still reported as running.
    pub fn running(&self) -> usize {
        self.jobs
            .values()
            .filter(|job| matches!(job.state, JobState::Running))
            .count()
    }

    /// Get a reference to a job's state.
    pub fn get_state(&self, id: &str) -> Option<JobState> {
        self.jobs.get(id).map(|j| j.state.clone())
    }

    /// Update a job's state.
    fn update(&mut self, id: &str, state: JobState) {
        let is_terminal = state.is_terminal();
        if let Some(job) = self.jobs.get_mut(id) {
            job.state = state;
        }
        if is_terminal {
            // Terminal: the job is no longer cancellable, so deregister it. A
            // stale routable entry would make a later cancel look handled while
            // nothing owns the task any more.
            self.cancels.remove(id);
            self.completed_notify.notify_one();
        }
    }

    /// Remove stale jobs (older than TTL).
    fn cleanup(&mut self) {
        // checked_sub: when the elapsed time since the Instant epoch is less than
        // JOB_TTL (e.g. shortly after system boot), `now - TTL` would underflow and
        // panic. In that case no job can possibly be older than the TTL, so keep them
        // all (cutoff = now → `created_at > now` is false for every job, but that's
        // fine — nothing is evicted, which is correct since nothing is stale).
        let cutoff = Instant::now()
            .checked_sub(JOB_TTL)
            .unwrap_or_else(Instant::now);
        self.jobs.retain(|_, job| job.created_at > cutoff);
        // A job evicted here can no longer be cancelled; drop its registration
        // so the task table does not keep a stale routable entry (Goal 412).
        let live: Vec<String> = self.jobs.keys().cloned().collect();
        self.cancels.retain(|id, _| live.contains(id));
    }

    /// Remove all jobs immediately.
    ///
    /// Dropping a job deregisters its cancel handle, so nothing is left
    /// routable. Clearing a table that still has *running* jobs is a silent
    /// process leak (issue #177) — it is reported here, and destroy paths go
    /// through [`reclaim_all`] instead.
    pub fn clear(&mut self) {
        let running = self.running();
        if running > 0 {
            tracing::warn!(
                target: crate::process::CANCEL_TARGET,
                scope = %self.scope,
                running,
                "background job table cleared with running jobs — their processes were not liquidated by this call"
            );
        }
        self.jobs.clear();
        self.cancels.clear();
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

    /// Set an agent-requested loop control (e.g. `Stop`). Replaces any prior
    /// pending control. Read and consumed by the loop arbiter between turns.
    pub fn set_loop_control(&mut self, ctrl: LoopControl) {
        self.loop_control = Some(ctrl);
    }

    /// Take (consume) any pending loop-control request. Returns `None` when
    /// nothing is pending.
    pub fn take_loop_control(&mut self) -> Option<LoopControl> {
        self.loop_control.take()
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
    ///
    /// If a terminal job remains after this removal, the completion notify is
    /// re-armed. `update()` uses `notify_one()`, which stores a single permit:
    /// when ≥2 jobs reach a terminal state before a waiter polls, the second
    /// `notify_one()` is a no-op, so a waiter that consumes the one permit
    /// would never wake for the remaining job. Re-arming here guarantees every
    /// completed job has a scheduled wake until it is drained (goal-379).
    pub fn take_completed(&mut self) -> Option<(String, String)> {
        let id = self.jobs.iter().find_map(|(id, job)| match &job.state {
            JobState::Completed { .. }
            | JobState::Failed { .. }
            | JobState::TimedOut
            | JobState::Cancelled => Some(id.clone()),
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
            JobState::Cancelled => "cancelled".to_string(),
            JobState::Running => unreachable!(), // filtered above
        };
        if self.jobs.values().any(|j| j.state.is_terminal()) {
            self.completed_notify.notify_one();
        }
        Some((id, output))
    }
}

/// Outcome of destroy-time liquidation of a background job table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReclaimReport {
    /// Jobs that were cancelled by this call.
    pub cancelled: usize,
    /// Jobs still reported as running when the grace period expired. Non-zero
    /// is a liquidation failure and must be surfaced (WARNING), never assumed
    /// away — the whole point of #177 is that a silent leak looked like success.
    pub unreclaimed: usize,
}

/// Destroy-time liquidation (Goal 412 / issue #177).
///
/// Cancels every running job, waits (bounded) for each job loop to report a
/// terminal state — which it only does after `killpg`-ing its process group —
/// and then drops the table. Returns what happened so the caller can be loud
/// about a failure; this function itself never claims more than it observed.
pub async fn reclaim_all(
    manager: &Arc<Mutex<BackgroundJobManager>>,
    grace: Duration,
) -> ReclaimReport {
    let cancelled = {
        let mut mgr = manager.lock().await;
        mgr.cancel_all()
    };
    let deadline = Instant::now() + grace;
    let unreclaimed = loop {
        let running = manager.lock().await.running();
        if running == 0 || Instant::now() >= deadline {
            break running;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    manager.lock().await.clear();
    ReclaimReport {
        cancelled,
        unreclaimed,
    }
}

/// The `run_background` tool: spawn a command and return immediately.
#[derive(Clone)]
pub struct RunBackground {
    root: PathBuf,
    manager: Arc<Mutex<BackgroundJobManager>>,
    /// Optional shared transport (issue #31 §3). When `Some`, the command
    /// executes via `transport.exec_shell` (container tier) instead of the
    /// host `/bin/sh` — so background jobs live inside the session's
    /// sandbox and die with it (`ToolTransport::destroy`).
    transport: Option<Arc<dyn crate::tools::transport::ToolTransport>>,
}

impl RunBackground {
    pub fn new(root: impl Into<PathBuf>, manager: Arc<Mutex<BackgroundJobManager>>) -> Self {
        Self {
            root: root.into(),
            manager,
            transport: None,
        }
    }

    /// Bind execution to a shared transport (Goal 403 / issue #31 §3).
    pub fn with_transport(
        mut self,
        transport: Arc<dyn crate::tools::transport::ToolTransport>,
    ) -> Self {
        self.transport = Some(transport);
        self
    }
}

#[async_trait]
impl Tool for RunBackground {
    /// Goal 394: session-level fork — rewire the shared background-job
    /// manager so jobs spawned in one session are invisible to another.
    fn fork_box(&self, state: &SessionToolState) -> Option<Arc<dyn Tool>> {
        let mut forked = self.clone();
        forked.manager = state.bg_manager.clone();
        Some(Arc::new(forked))
    }

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

        // Collect optional env overrides (validated for both branches).
        let mut env_pairs: Vec<(String, String)> = Vec::new();
        if let Some(env_map) = args.get("env").and_then(|v| v.as_object()) {
            for (key, val) in env_map {
                let val_str = val.as_str().ok_or_else(|| Error::BadToolArgs {
                    name: "run_background".to_string(),
                    message: format!("env value for `{key}` must be a string, got {:?}", val),
                })?;
                env_pairs.push((key.clone(), val_str.to_string()));
            }
        }

        // Issue #31 §3: transport-bound execution (container tier) — the
        // command runs inside the session's sandbox environment, so it dies
        // with the environment (transport destroy) instead of leaking on
        // the host.
        if let Some(transport) = self.transport.clone() {
            let mut manager = self.manager.lock().await;
            manager.cleanup();
            let job_id = manager.insert(Job::running());
            let cancel = manager.cancel_handle(&job_id);
            drop(manager);

            let manager_clone = self.manager.clone();
            let job_id_clone = job_id.clone();
            let command_display = command.to_string();
            let command = command.to_string();
            tokio::spawn(async move {
                let exec = async {
                    match transport
                        .exec_shell(
                            &command,
                            &cwd,
                            &env_pairs,
                            Duration::from_secs(timeout_secs),
                            MAX_OUTPUT_BYTES,
                        )
                        .await
                    {
                        Ok(r) => match r.failure {
                            Some(_) => JobState::Failed { message: r.stderr },
                            None => JobState::Completed {
                                stdout: r.stdout,
                                stderr: r.stderr,
                                exit_code: r.exit_code.unwrap_or(-1),
                            },
                        },
                        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => JobState::TimedOut,
                        Err(e) => JobState::Failed {
                            message: format!("exec failed: {e}"),
                        },
                    }
                };
                // Goal 412: the job is cancellable here too. Dropping the exec
                // future is the only way to stop a sandbox execution early —
                // the local tier reaps its child via `kill_on_drop`, and the
                // container tier's own `destroy()` removes the environment
                // right after (destroy is the next step of `destroy_environment`).
                let result = match &cancel {
                    Some(handle) => {
                        tokio::select! {
                            result = exec => result,
                            _ = handle.cancelled() => {
                                handle.note_stage("cancelled-in-sandbox");
                                JobState::Cancelled
                            }
                        }
                    }
                    None => exec.await,
                };
                let mut mgr = manager_clone.lock().await;
                mgr.update(&job_id_clone, result);
            });

            return Ok(json!({
                "job_id": job_id,
                "status": "spawned",
                "message": format!("Background job `{}` spawned. Use `check_background` with job_id `{}` to retrieve output.", command_display, job_id)
            })
            .to_string());
        }

        // Host path (none / policy tiers): the default tier scrubs
        // credential-shaped host env (issue #89) — a background `printenv`
        // must not leak the service's API keys / JWT secret either.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(command);
        cmd.current_dir(&cwd);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        crate::tools::transport::scrub_child_env(&mut cmd);

        // Apply optional env overrides
        for (key, val) in &env_pairs {
            cmd.env(key, val);
        }

        // Spawn the process. Goal 412: through the single spawn entry, so the
        // shell leads its own process group and a timeout/cancel can reap the
        // whole tree instead of just `sh` (issue #165).
        let mut child = crate::process::spawn_owned(&mut cmd).map_err(|e| Error::Tool {
            name: "run_background".into(),
            call_id: None,
            message: format!("spawn failed: {e}"),
        })?;

        let stdout_handle = child.stdout.take();
        let stderr_handle = child.stderr.take();

        // Generate a job ID and store it (the insert registers the job's cancel
        // handle in the task table).
        let mut manager = self.manager.lock().await;
        manager.cleanup();
        let job_id = manager.insert(Job::running());
        let cancel = manager.cancel_handle(&job_id);
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
                cancel,
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
#[derive(Clone)]
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
    /// Goal 394: session-level fork — see [`Tool::fork_box`] on `RunBackground`.
    fn fork_box(&self, state: &SessionToolState) -> Option<Arc<dyn Tool>> {
        let mut forked = self.clone();
        forked.manager = state.bg_manager.clone();
        Some(Arc::new(forked))
    }

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
            Some(JobState::Cancelled) => Ok(json!({
                "job_id": job_id,
                "status": "cancelled",
                "message": "Job was cancelled and its process group reaped."
            })
            .to_string()),
        }
    }
}

/// How a job's process finished.
enum WaitOutcome {
    Exited(std::io::Result<std::process::ExitStatus>),
    TimedOut,
    Cancelled,
}

/// Mirror of the bytes a reader has captured so far, so a drain that has to be
/// abandoned still returns what was read.
type OutputMirror = std::sync::Arc<std::sync::Mutex<Vec<u8>>>;

/// Read one stream to EOF (or to the output cap), draining the rest so a
/// chatty command never blocks on a full pipe.
async fn read_stream<R>(reader: Option<R>, label: &'static str, mirror: OutputMirror) -> String
where
    R: AsyncReadExt + Unpin,
{
    let mut reader = match reader {
        Some(reader) => reader,
        None => return String::new(),
    };
    let mut out = String::new();
    let mut buf = [0u8; 8192];
    let mut total = 0usize;
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if total + n > MAX_OUTPUT_BYTES {
                    let take = MAX_OUTPUT_BYTES.saturating_sub(total);
                    out.push_str(&String::from_utf8_lossy(&buf[..take]));
                    out.push_str(&format!("\n... [{label} truncated]"));
                    // Keep draining so the writer is never wedged on a full pipe.
                    let _ = tokio::io::copy(&mut reader, &mut tokio::io::sink()).await;
                    break;
                }
                out.push_str(&String::from_utf8_lossy(&buf[..n]));
                total += n;
                let mut mirrored = mirror
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if mirrored.len() < MAX_OUTPUT_BYTES {
                    mirrored.extend_from_slice(&buf[..n]);
                }
            }
            Err(_) => break,
        }
    }
    out
}

fn mirrored_text(mirror: &OutputMirror) -> String {
    let mirrored = mirror
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    String::from_utf8_lossy(&mirrored).into_owned()
}

/// Bounded drain of a reader task: once the process group is gone EOF should
/// arrive, but a descendant that escaped the group (`setsid`) can hold the
/// write end open, so we never park forever — the mirror returns whatever was
/// already captured.
async fn drain_reader(
    task: tokio::task::JoinHandle<String>,
    mirror: &OutputMirror,
    label: &'static str,
) -> String {
    match timeout(READER_DRAIN_GRACE, task).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            tracing::warn!(
                target: crate::process::CANCEL_TARGET,
                stream = label,
                error = %e,
                "background job reader task failed; captured output may be incomplete"
            );
            mirrored_text(mirror)
        }
        Err(_) => {
            tracing::warn!(
                target: crate::process::CANCEL_TARGET,
                stream = label,
                grace_ms = READER_DRAIN_GRACE.as_millis() as u64,
                "background job reader did not reach EOF after the process group died; keeping the bytes captured so far"
            );
            mirrored_text(mirror)
        }
    }
}

/// Wait for a background process to finish, capturing its output.
///
/// The reader tasks are spawned **before** the waiter is polled: a command that
/// writes more than the pipe buffer blocks in `write(2)` until someone drains
/// the pipe, so waiting for the process while nobody reads deadlocks the job
/// until its timeout (issue #164). Timeout and cancellation are handled here so
/// both end the whole process group through [`crate::process::terminate_group`]
/// (issue #165) and leave a job state the caller can act on.
async fn run_background_job(
    mut child: tokio::process::Child,
    stdout_opt: Option<tokio::process::ChildStdout>,
    stderr_opt: Option<tokio::process::ChildStderr>,
    job_timeout: Duration,
    cancel: Option<CancelHandle>,
) -> JobState {
    let stdout_mirror: OutputMirror = OutputMirror::default();
    let stderr_mirror: OutputMirror = OutputMirror::default();
    let stdout_task = tokio::spawn(read_stream(
        stdout_opt,
        "stdout",
        OutputMirror::clone(&stdout_mirror),
    ));
    let stderr_task = tokio::spawn(read_stream(
        stderr_opt,
        "stderr",
        OutputMirror::clone(&stderr_mirror),
    ));

    if let Some(handle) = &cancel {
        handle.note_stage("running");
    }
    let token = match &cancel {
        Some(handle) => handle.token(),
        None => tokio_util::sync::CancellationToken::new(),
    };

    let outcome = tokio::select! {
        status = child.wait() => WaitOutcome::Exited(status),
        _ = tokio::time::sleep(job_timeout) => WaitOutcome::TimedOut,
        _ = token.cancelled() => WaitOutcome::Cancelled,
    };

    match &outcome {
        WaitOutcome::Exited(_) => {
            if let Some(handle) = &cancel {
                handle.note_stage("draining-output");
            }
        }
        WaitOutcome::TimedOut => {
            if let Some(handle) = &cancel {
                handle.note_stage("timed-out");
            }
            crate::process::terminate_group(&mut child).await;
        }
        WaitOutcome::Cancelled => {
            if let Some(handle) = &cancel {
                handle.note_stage("reaping-process-group");
            }
            crate::process::terminate_group(&mut child).await;
        }
    }

    let (stdout, stderr) = tokio::join!(
        drain_reader(stdout_task, &stdout_mirror, "stdout"),
        drain_reader(stderr_task, &stderr_mirror, "stderr"),
    );

    match outcome {
        WaitOutcome::Exited(Ok(status)) => JobState::Completed {
            stdout,
            stderr,
            exit_code: status.code().unwrap_or(-1),
        },
        WaitOutcome::Exited(Err(e)) => JobState::Failed {
            message: format!("wait failed: {e}"),
        },
        WaitOutcome::TimedOut => JobState::TimedOut,
        WaitOutcome::Cancelled => JobState::Cancelled,
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

    /// Issue #89: the legacy **host path** (`RunBackground` built without a
    /// transport) must scrub credential-shaped env too — a background
    /// `printenv` is otherwise the same exfiltration channel as `Bash`.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // std env lock is fine: only same-crate tests contend
    async fn run_background_host_path_scrubs_sensitive_env() {
        let _lock = crate::test_util::env_lock();
        // SAFETY: process-global env mutation, serialised by `env_lock`.
        let prev_key = std::env::var_os("RECURSIVE_API_KEY");
        let prev_benign = std::env::var_os("BACKGROUND_BENIGN_VAR");
        unsafe {
            std::env::set_var("RECURSIVE_API_KEY", "leak-me");
            std::env::set_var("BACKGROUND_BENIGN_VAR", "keepme");
        }

        let tmp = TempDir::new().unwrap();
        let manager = Arc::new(Mutex::new(BackgroundJobManager::new()));
        let run_tool = RunBackground::new(tmp.path(), manager.clone());
        let check_tool = CheckBackground::new(manager.clone());

        let result = run_tool
            .execute(json!({
                "command": "printenv | grep -c RECURSIVE; printenv BACKGROUND_BENIGN_VAR"
            }))
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        let job_id = parsed["job_id"].as_str().unwrap().to_string();

        let parsed = poll_until_done(&check_tool, &job_id, Duration::from_secs(10)).await;
        assert_eq!(parsed["status"], "completed");
        let stdout = parsed["stdout"].as_str().unwrap();
        assert!(
            stdout.lines().next() == Some("0"),
            "background printenv leaked RECURSIVE_* env: {stdout}"
        );
        assert!(
            stdout.contains("keepme"),
            "benign env was scrubbed: {stdout}"
        );

        // Restore the process env for the rest of the suite.
        unsafe {
            match prev_key {
                Some(v) => std::env::set_var("RECURSIVE_API_KEY", v),
                None => std::env::remove_var("RECURSIVE_API_KEY"),
            }
            match prev_benign {
                Some(v) => std::env::set_var("BACKGROUND_BENIGN_VAR", v),
                None => std::env::remove_var("BACKGROUND_BENIGN_VAR"),
            }
        }
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
    async fn two_completed_jobs_each_get_a_wake() {
        // goal-379: `update()` uses notify_one(), which stores a SINGLE
        // permit. When two jobs reach a terminal state before any waiter
        // polls, the two notify_one() calls coalesce into one wake. The
        // drainer consumes that wake, takes ONE job, and — without a re-arm —
        // the second job would be orphaned forever (the loop idles even
        // though a background job is finished). take_completed() must re-arm
        // the notify while terminal jobs remain, so each completed job gets
        // its own wake.
        let mut manager = BackgroundJobManager::new();
        let id_a = manager.insert(Job {
            state: JobState::Running,
            created_at: Instant::now(),
        });
        let id_b = manager.insert(Job {
            state: JobState::Running,
            created_at: Instant::now(),
        });
        manager.update(
            &id_a,
            JobState::Completed {
                stdout: "a".into(),
                stderr: "".into(),
                exit_code: 0,
            },
        );
        manager.update(
            &id_b,
            JobState::Completed {
                stdout: "b".into(),
                stderr: "".into(),
                exit_code: 0,
            },
        );

        let notify = manager.completed_notify();

        // First wake consumes the single coalesced permit.
        tokio::time::timeout(Duration::from_secs(5), notify.notified())
            .await
            .expect("first completed job must produce a wake");

        // Drain one job; the other is still terminal, so take_completed must
        // re-arm the notify for it.
        let (first_id, first_output) = manager.take_completed().expect("first completed job");
        assert!(first_output.contains("exit code: 0"));

        // Second wake must fire for the remaining job (would time out on the
        // pre-fix code, where the coalesced permit was already consumed).
        tokio::time::timeout(Duration::from_secs(5), notify.notified())
            .await
            .expect("second completed job must also produce a wake");
        let (second_id, second_output) = manager.take_completed().expect("second completed job");
        assert!(second_output.contains("exit code: 0"));
        assert_ne!(first_id, second_id, "each job must be drained exactly once");

        // Queue fully drained; nothing left to take.
        assert!(manager.take_completed().is_none());
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

    #[tokio::test]
    async fn cleanup_does_not_panic_on_young_process() {
        let mut mgr = BackgroundJobManager::new();
        // Insert a freshly-created job (created_at = now). A job created in
        // this process can never be older than JOB_TTL, so cleanup must keep
        // it — and it must not panic even when `now - JOB_TTL` would underflow
        // (the case where the Instant epoch is younger than the TTL, e.g.
        // shortly after system boot).
        let id = mgr.insert(Job {
            state: JobState::Running,
            created_at: Instant::now(),
        });

        // Must NOT panic even though `now - JOB_TTL` underflows on a young
        // epoch.
        mgr.cleanup();

        assert!(
            mgr.get_state(&id).is_some(),
            "freshly-created job must survive cleanup"
        );
    }

    // ── Goal 412 process/cancel contract ───────────────────────────────────

    /// `kill -0` probe: true while `pid` exists.
    fn pid_alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    async fn wait_gone(pid: u32, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            if !pid_alive(pid) {
                return true;
            }
            sleep(Duration::from_millis(20)).await;
        }
        !pid_alive(pid)
    }

    /// Read a pid marker file written by a spawned command.
    async fn read_pid(path: &std::path::Path) -> Option<u32> {
        for _ in 0..100 {
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Ok(pid) = text.trim().parse::<u32>() {
                    return Some(pid);
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
        None
    }

    /// Contract: a long-running job carries a cancel handle in the task table,
    /// and consuming it ends the job's process group while the job's output is
    /// being drained concurrently.
    ///
    /// Mutation guards (Scope 6):
    /// * drop the `token.cancelled()` arm of the waiter in
    ///   [`run_background_job`] → the job runs to completion → red
    ///   ("cancel 路径断裂");
    /// * replace `terminate_group` with `child.start_kill()` → the descendant
    ///   holding the pipe survives → red.
    #[tokio::test]
    async fn test_process_contract_cancel_handle() {
        let tmp = TempDir::new().unwrap();
        let manager = Arc::new(Mutex::new(BackgroundJobManager::new()));
        let run_tool = RunBackground::new(tmp.path(), manager.clone());
        let check_tool = CheckBackground::new(manager.clone());
        let pid_file = tmp.path().join("job.pid");

        // Floods far past the pipe buffer *and then* sleeps: the cancel can
        // only be observed if the readers are polled concurrently (issue #164).
        let command = format!(
            "echo $$ > '{}'; yes x | head -c 200000; exec sleep 60",
            pid_file.display()
        );
        let result = run_tool
            .execute(json!({"command": command, "timeout_secs": 3600}))
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        let job_id = parsed["job_id"].as_str().unwrap().to_string();

        let pid = read_pid(&pid_file)
            .await
            .expect("job should record its pid before sleeping");
        assert!(pid_alive(pid), "job process should be alive before cancel");

        // Route the cancel the way an edge would: by key through the
        // process-wide task table (no access to the handle object itself).
        let key = manager
            .lock()
            .await
            .cancel_handle(&job_id)
            .expect("a running job must be registered in the task table")
            .key()
            .to_string();
        assert!(key.contains(&job_id), "handle key names the job: {key}");
        assert!(
            crate::process::cancel_task(&key),
            "routed cancel must reach the job's handle"
        );

        let parsed = poll_until_done(&check_tool, &job_id, Duration::from_secs(10)).await;
        assert_eq!(
            parsed["status"], "cancelled",
            "cancelled job must report `cancelled`, got {parsed}"
        );
        assert!(
            wait_gone(pid, Duration::from_secs(5)).await,
            "job process {pid} survived cancellation"
        );
        assert!(
            manager.lock().await.get_state(&job_id).is_some(),
            "the cancelled job's row stays readable for check_background"
        );
    }

    /// Contract: destroy is liquidation — cancel, reap, then drop the table.
    ///
    /// Mutation guard (Scope 6, "destroy 漏清算 → 红"): make [`reclaim_all`]
    /// clear the table without cancelling, or skip the process-group kill, and
    /// the process is still alive here → red (issue #177).
    #[tokio::test]
    async fn test_process_contract_destroy_reclaim() {
        let tmp = TempDir::new().unwrap();
        let manager = Arc::new(Mutex::new(BackgroundJobManager::new()));
        let run_tool = RunBackground::new(tmp.path(), manager.clone());
        let pid_file = tmp.path().join("destroy.pid");

        let command = format!("echo $$ > '{}' && exec sleep 60", pid_file.display());
        run_tool
            .execute(json!({"command": command, "timeout_secs": 3600}))
            .await
            .unwrap();
        let pid = read_pid(&pid_file)
            .await
            .expect("job should record its pid");
        assert!(pid_alive(pid), "job process should be alive before destroy");

        let report = reclaim_all(&manager, DEFAULT_RECLAIM_GRACE).await;
        assert_eq!(report.cancelled, 1, "the running job must be cancelled");
        assert_eq!(
            report.unreclaimed, 0,
            "liquidation must observe every job reaping its group"
        );
        assert!(
            wait_gone(pid, Duration::from_secs(5)).await,
            "companion process {pid} survived destroy — destroy must liquidate (issue #177)"
        );
        assert!(
            manager.lock().await.get_state("bg-1").is_none(),
            "reclaim drops the job table"
        );

        // An empty table reclaims cleanly.
        let empty = reclaim_all(&manager, Duration::from_millis(50)).await;
        assert_eq!(
            empty,
            ReclaimReport {
                cancelled: 0,
                unreclaimed: 0
            }
        );
    }

    /// Contract: liquidation is counted, not assumed. A job that never reports
    /// back (no job loop owns it) must surface as `unreclaimed` so the caller
    /// can be loud about the leak.
    ///
    /// Mutation guard: make [`reclaim_all`] report `unreclaimed: 0`
    /// unconditionally and this goes red.
    #[tokio::test]
    async fn reclaim_reports_jobs_that_never_reap() {
        let manager = Arc::new(Mutex::new(BackgroundJobManager::new()));
        manager.lock().await.insert(Job::running());

        let report = reclaim_all(&manager, Duration::from_millis(50)).await;
        assert_eq!(report.cancelled, 1);
        assert_eq!(
            report.unreclaimed, 1,
            "a job that never reports a terminal state is an unproven liquidation"
        );
    }

    /// Issue #164 regression: output far beyond the pipe buffer must not
    /// deadlock the job. Before the fix the waiter was polled alone — the child
    /// blocked in `write(2)` on a full pipe and the job wedged until its
    /// timeout.
    ///
    /// Mutation guard: poll `child.wait()` without the concurrent reader tasks
    /// and this test times out instead of seeing `completed`.
    #[tokio::test]
    async fn background_output_beyond_pipe_buffer_does_not_deadlock() {
        let tmp = TempDir::new().unwrap();
        let manager = Arc::new(Mutex::new(BackgroundJobManager::new()));
        let run_tool = RunBackground::new(tmp.path(), manager.clone());
        let check_tool = CheckBackground::new(manager.clone());

        // ~400 KiB on stdout and ~200 KiB on stderr, both >> the 64 KiB pipe
        // buffer, while the per-stream capture cap is 128 KiB.
        let result = run_tool
            .execute(json!({
                "command": "yes x | head -c 400000; yes y | head -c 200000 1>&2; echo done",
                "timeout_secs": 60,
            }))
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        let job_id = parsed["job_id"].as_str().unwrap().to_string();

        let parsed = poll_until_done(&check_tool, &job_id, Duration::from_secs(20)).await;
        assert_eq!(
            parsed["status"], "completed",
            "the job must finish, not wedge on a full pipe: {parsed}"
        );
        assert_eq!(parsed["exit_code"], 0);
        let stdout = parsed["stdout"].as_str().unwrap();
        assert!(
            stdout.contains("[stdout truncated]"),
            "the 128 KiB cap must have truncated 400 KiB of output"
        );
        assert!(
            stdout.len() > 64 * 1024,
            "the reader must have captured well past the pipe buffer, got {} bytes",
            stdout.len()
        );
        assert!(
            parsed["stderr"]
                .as_str()
                .unwrap()
                .contains("[stderr truncated]"),
            "stderr must be drained and capped too"
        );
    }

    /// Issue #165 regression: a timeout kills the shell's descendants too.
    ///
    /// Mutation guard: `child.start_kill()` instead of
    /// [`crate::process::terminate_group`] leaves the `sleep` grandchild alive
    /// → red.
    #[tokio::test]
    async fn background_timeout_kills_descendants() {
        let tmp = TempDir::new().unwrap();
        let manager = Arc::new(Mutex::new(BackgroundJobManager::new()));
        let run_tool = RunBackground::new(tmp.path(), manager.clone());
        let check_tool = CheckBackground::new(manager.clone());
        let pid_file = tmp.path().join("pids");

        // `sh` spawns a grandchild and waits, so the grandchild is *not* the
        // process the job directly waits on.
        let command = format!("sleep 60 & echo $$ $! > '{}'; wait", pid_file.display());
        let result = run_tool
            .execute(json!({"command": command, "timeout_secs": 1}))
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        let job_id = parsed["job_id"].as_str().unwrap().to_string();

        let mut pids: Option<(u32, u32)> = None;
        for _ in 0..100 {
            if let Ok(text) = std::fs::read_to_string(&pid_file) {
                let mut parts = text.split_whitespace();
                if let (Some(a), Some(b)) = (parts.next(), parts.next()) {
                    if let (Ok(a), Ok(b)) = (a.parse::<u32>(), b.parse::<u32>()) {
                        pids = Some((a, b));
                        break;
                    }
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
        let (leader, descendant) = pids.expect("child should record both pids");

        let parsed = poll_until_done(&check_tool, &job_id, Duration::from_secs(10)).await;
        assert_eq!(parsed["status"], "timed_out");
        assert!(
            wait_gone(leader, Duration::from_secs(5)).await,
            "timed-out leader {leader} still alive"
        );
        assert!(
            wait_gone(descendant, Duration::from_secs(5)).await,
            "descendant {descendant} survived the timeout — the group was not killed (issue #165)"
        );
    }
}
