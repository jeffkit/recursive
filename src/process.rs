//! Process ownership contract (Goal 412 / issues #198, #188).
//!
//! Three judgment cases in this repo shared one root cause: a child process was
//! created and nothing structurally owned it afterwards.
//!
//! * #164 — `run_background` deadlocked as soon as a command wrote more than
//!   the pipe buffer: the waiter was polled alone, so nobody drained the pipe.
//! * #165 — a timeout killed `sh`, leaving everything `sh` had spawned running.
//! * #177 — `destroy_environment` reset the job table and let the host
//!   processes keep running.
//!
//! This module is the single place that ownership is built:
//!
//! * **Process groups.** Every subprocess is spawned as the leader of its own
//!   group ([`spawn_grouped`] / [`spawn_owned`]), so ending it means `killpg`
//!   — the whole tree, not just the shell ([`terminate_group`]).
//! * **Cancel handles.** Long-running work is registered as a
//!   [`TaskRegistration`] in [`cancel_tasks`], the process-wide *task table*:
//!   cancellation becomes an operation on an addressable object (key or
//!   scope) instead of a scattered `bool`.
//! * **Observability.** Every cancellation leaves a structured event on the
//!   [`CANCEL_TARGET`] tracing target — who, when, what, and how far the task
//!   had got — plus an in-process [`CancelEvent`] history for callers/tests.
//!
//! Unix-only by nature: process groups are POSIX. On Windows [`kill_group`]
//! returns `false` and [`terminate_group`] falls back to the pre-existing
//! `Child::start_kill` path, i.e. no new Windows behaviour is introduced.
//!
//! Audited and deliberately unchanged: one-shot `Command::output()` calls
//! (spawned and reaped inside std), external hook children in `hooks::external`
//! (the handle is consumed by `wait_with_output`, so a group cannot be
//! signalled from that timeout arm without restructuring the loop), and the
//! container / e2b / SSH *remote* commands (no local child to own).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Tracing target every cancellation event is recorded on.
pub const CANCEL_TARGET: &str = "recursive::tasks";

/// Stage a task reports before it starts doing work.
const STAGE_STARTING: &str = "starting";

/// How many cancellation events the in-process history keeps. Bounded so a
/// pathological cancel loop cannot grow memory without limit; the tracing
/// stream remains the unbounded record.
const MAX_CANCEL_EVENTS: usize = 64;

// ---------------------------------------------------------------------------
// Cancel handles: the task table
// ---------------------------------------------------------------------------

/// Kinds of long-running work that carry a cancel handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelKind {
    /// A `run_background` job.
    BackgroundJob,
    /// A foreground shell exec (`Bash` → `LocalTransport::exec_shell`).
    Exec,
    /// An MCP stdio server process.
    McpServer,
}

impl CancelKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CancelKind::BackgroundJob => "background-job",
            CancelKind::Exec => "exec",
            CancelKind::McpServer => "mcp-server",
        }
    }
}

/// Why a task was cancelled. Kept as data so a consumer (log, test, a future
/// ACP reply) can tell an operator's stop from destroy-time liquidation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReason {
    /// An explicit cancel routed to this one task.
    Explicit,
    /// A scope-wide cancel (execution / session root).
    Scope,
    /// Environment teardown — destroy is liquidation, not a table reset.
    Destroy,
}

impl CancelReason {
    pub fn as_str(self) -> &'static str {
        match self {
            CancelReason::Explicit => "explicit",
            CancelReason::Scope => "scope",
            CancelReason::Destroy => "destroy",
        }
    }
}

/// One structured cancellation record (Goal 412 "取消可观测").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelEvent {
    /// Address of the cancelled task in the task table.
    pub key: String,
    /// What kind of work it was.
    pub kind: CancelKind,
    /// Execution / session root scope, when the task declared one.
    pub scope: Option<String>,
    /// Who asked (explicit cancel, scope sweep, destroy).
    pub reason: CancelReason,
    /// How far the task had got when the cancel landed.
    pub stage: String,
    /// Wall-clock time of the cancellation (unix epoch millis).
    pub at_ms: u128,
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default()
}

type SharedLog = Arc<Mutex<Vec<CancelEvent>>>;

fn lock_log(log: &SharedLog) -> std::sync::MutexGuard<'_, Vec<CancelEvent>> {
    // A poisoned log means a panic happened while holding it; recovering the
    // guard keeps the record intact rather than silently dropping events.
    log.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A registered handle on one cancellable long-running task.
///
/// Cheap to clone (token and stage are shared), so the owner of the task can
/// select on it while an edge holds the registry's copy.
#[derive(Clone)]
pub struct CancelHandle {
    key: String,
    kind: CancelKind,
    scope: Option<String>,
    stage: Arc<Mutex<String>>,
    token: CancellationToken,
    log: SharedLog,
}

impl CancelHandle {
    /// Address of this task in the table it was registered in.
    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn kind(&self) -> CancelKind {
        self.kind
    }

    pub fn scope(&self) -> Option<&str> {
        self.scope.as_deref()
    }

    /// The token the task's owner selects on.
    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    /// Await cancellation (returns immediately once cancelled).
    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }

    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Record how far the task has got. Ignored once cancelled, so the event
    /// keeps the stage the task had reached when the cancel landed.
    pub fn note_stage(&self, stage: impl Into<String>) {
        if self.token.is_cancelled() {
            return;
        }
        *self
            .stage
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = stage.into();
    }

    pub fn stage(&self) -> String {
        self.stage
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Cancel explicitly. Returns `false` when it was already cancelled — the
    /// record is written exactly once, by whoever got there first.
    pub fn cancel(&self) -> bool {
        self.cancel_with(CancelReason::Explicit)
    }

    /// Cancel with an explicit reason (see [`CancelRegistry`] for the sweeps).
    pub fn cancel_with(&self, reason: CancelReason) -> bool {
        if self.token.is_cancelled() {
            return false;
        }
        let stage = self.stage();
        let at_ms = unix_ms();
        // Structured, greppable record: `RUST_LOG=recursive::tasks=warn` shows
        // every cancellation with who / what / where.
        tracing::warn!(
            target: CANCEL_TARGET,
            task = %self.key,
            kind = %self.kind.as_str(),
            scope = %self.scope.as_deref().unwrap_or("-"),
            reason = %reason.as_str(),
            stage = %stage,
            at_ms,
            "task cancelled"
        );
        {
            let mut log = lock_log(&self.log);
            log.push(CancelEvent {
                key: self.key.clone(),
                kind: self.kind,
                scope: self.scope.clone(),
                reason,
                stage,
                at_ms,
            });
            if log.len() > MAX_CANCEL_EVENTS {
                log.remove(0);
            }
        }
        self.token.cancel();
        true
    }
}

/// The task table: every cancellable long-running task, addressable by key or
/// by scope, so a cancel channel can route to a handle instead of threading a
/// flag through the call stack.
#[derive(Default)]
pub struct CancelRegistry {
    next: u64,
    handles: HashMap<String, CancelHandle>,
    log: SharedLog,
}

impl CancelRegistry {
    pub fn new() -> Self {
        Self {
            next: 1,
            handles: HashMap::new(),
            log: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Register a task and return its handle. `scope` is the execution /
    /// session root the task belongs to (opaque string — tenancy itself is
    /// resolved at the serving edge, never in the kernel).
    pub fn register(&mut self, kind: CancelKind, scope: Option<&str>) -> CancelHandle {
        let seq = self.next;
        self.next = self.next.saturating_add(1);
        let key = format!("{}-{seq}", kind.as_str());
        self.register_inner(kind, scope, key)
    }

    /// Register a task under an explicit key. The caller guarantees the key is
    /// unique within this table — used where the key is the task's own
    /// identity (a background job id), so the cancellation record names it.
    pub fn register_keyed(
        &mut self,
        kind: CancelKind,
        scope: Option<&str>,
        key: impl Into<String>,
    ) -> CancelHandle {
        self.register_inner(kind, scope, key.into())
    }

    fn register_inner(
        &mut self,
        kind: CancelKind,
        scope: Option<&str>,
        key: String,
    ) -> CancelHandle {
        let handle = CancelHandle {
            key: key.clone(),
            kind,
            scope: scope.map(str::to_string),
            stage: Arc::new(Mutex::new(STAGE_STARTING.to_string())),
            token: CancellationToken::new(),
            log: Arc::clone(&self.log),
        };
        self.handles.insert(key, handle.clone());
        handle
    }

    /// Cancel one task by key.
    pub fn cancel(&mut self, key: &str) -> bool {
        match self.handles.get(key) {
            Some(handle) => handle.cancel(),
            None => {
                tracing::debug!(
                    target: CANCEL_TARGET,
                    task = key,
                    "cancel routed to an unknown task"
                );
                false
            }
        }
    }

    /// Cancel every task registered under `scope`; returns how many were
    /// newly cancelled.
    pub fn cancel_scope(&mut self, scope: &str) -> usize {
        self.handles
            .values()
            .filter(|h| h.scope() == Some(scope))
            .filter(|h| h.cancel_with(CancelReason::Scope))
            .count()
    }

    /// Deregister a finished task. Returns `true` when it was still tracked.
    pub fn finish(&mut self, key: &str) -> bool {
        self.handles.remove(key).is_some()
    }

    /// Number of tasks currently tracked.
    pub fn active(&self) -> usize {
        self.handles.len()
    }

    /// Number of tracked tasks under `scope`.
    pub fn active_in_scope(&self, scope: &str) -> usize {
        self.handles
            .values()
            .filter(|h| h.scope() == Some(scope))
            .count()
    }

    /// The cancellation record (oldest first, bounded).
    pub fn events(&self) -> Vec<CancelEvent> {
        lock_log(&self.log).clone()
    }
}

fn lock_tasks() -> std::sync::MutexGuard<'static, CancelRegistry> {
    cancel_tasks()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The process-wide task table.
///
/// Long-running work registers here ([`TaskRegistration::spawn`]); a channel
/// that wants to end work — an HTTP DELETE, a future ACP cancel (#200), an
/// operator stop — resolves the handle here and lets the owning task reap its
/// process group. The table holds no tenancy: scopes are opaque strings handed
/// in by the edge (owner ruling 2026-10-18: tenancy stays out of the kernel).
pub fn cancel_tasks() -> &'static Mutex<CancelRegistry> {
    static TASKS: OnceLock<Mutex<CancelRegistry>> = OnceLock::new();
    TASKS.get_or_init(|| Mutex::new(CancelRegistry::new()))
}

/// Cancel one registered task by key (routing edge).
pub fn cancel_task(key: &str) -> bool {
    lock_tasks().cancel(key)
}

/// Cancel every registered task under `scope` (routing edge).
pub fn cancel_scope(scope: &str) -> usize {
    lock_tasks().cancel_scope(scope)
}

/// Deregister a task from the process-wide table.
pub fn finish_task(key: &str) -> bool {
    lock_tasks().finish(key)
}

/// Number of tasks currently registered in the process-wide table.
pub fn active_tasks() -> usize {
    lock_tasks().active()
}

/// A task's registration in the process-wide table.
///
/// Dropping it deregisters the task, so the registration tracks the *lifetime*
/// of the task rather than of a code path: an exec future dropped mid-flight
/// (or a job row cleared at destroy) cannot leave a stale, still-routable entry
/// behind, and a task that never deregisters stays visibly active.
#[derive(Debug)]
pub struct TaskRegistration {
    handle: CancelHandle,
}

impl TaskRegistration {
    /// Register a task in the process-wide table.
    pub fn spawn(kind: CancelKind, scope: Option<&str>) -> Self {
        let handle = lock_tasks().register(kind, scope);
        Self { handle }
    }

    /// Register a task under an explicit key (its own identity, e.g. a
    /// background job id qualified by the owning job table).
    pub fn spawn_keyed(kind: CancelKind, scope: Option<&str>, key: impl Into<String>) -> Self {
        let handle = lock_tasks().register_keyed(kind, scope, key);
        Self { handle }
    }

    pub fn key(&self) -> &str {
        self.handle.key()
    }

    pub fn handle(&self) -> &CancelHandle {
        &self.handle
    }

    pub fn kind(&self) -> CancelKind {
        self.handle.kind()
    }

    pub fn token(&self) -> CancellationToken {
        self.handle.token()
    }

    /// Await cancellation of this task.
    pub async fn cancelled(&self) {
        self.handle.cancelled().await;
    }

    pub fn is_cancelled(&self) -> bool {
        self.handle.is_cancelled()
    }

    pub fn note_stage(&self, stage: impl Into<String>) {
        self.handle.note_stage(stage);
    }

    pub fn stage(&self) -> String {
        self.handle.stage()
    }

    pub fn cancel(&self) -> bool {
        self.handle.cancel()
    }

    pub fn cancel_with(&self, reason: CancelReason) -> bool {
        self.handle.cancel_with(reason)
    }
}

impl std::fmt::Debug for CancelHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelHandle")
            .field("key", &self.key)
            .field("kind", &self.kind)
            .field("scope", &self.scope)
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl Drop for TaskRegistration {
    fn drop(&mut self) {
        finish_task(self.handle.key());
    }
}

// ---------------------------------------------------------------------------
// Process groups: one spawn entry, one kill entry
// ---------------------------------------------------------------------------

/// Signals [`signal_group`] can send to a whole process group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupSignal {
    Terminate,
    Kill,
}

impl GroupSignal {
    pub fn as_str(self) -> &'static str {
        match self {
            GroupSignal::Terminate => "SIGTERM",
            GroupSignal::Kill => "SIGKILL",
        }
    }
}

/// Make `cmd` the leader of its own process group (pgid == child pid), so the
/// child's descendants can be signalled as a unit.
#[cfg(unix)]
pub fn attach_group(cmd: &mut Command) {
    cmd.process_group(0);
}

/// Windows has no process groups: deliberately a no-op, so this contract
/// introduces no new Windows behaviour.
#[cfg(not(unix))]
pub fn attach_group(_cmd: &mut Command) {}

/// Spawn a child that leads its own process group.
pub fn spawn_grouped(cmd: &mut Command) -> std::io::Result<tokio::process::Child> {
    attach_group(cmd);
    cmd.spawn()
}

/// Spawn a child that leads its own process group *and* is killed if the
/// handle is dropped — the entry point for exec / background work, where a
/// half-finished caller must never leave the tree running.
pub fn spawn_owned(cmd: &mut Command) -> std::io::Result<tokio::process::Child> {
    cmd.kill_on_drop(true);
    spawn_grouped(cmd)
}

/// Signal the whole process group led by `pid`.
///
/// Returns `true` when the signal was delivered. A group that is already gone
/// (`ESRCH`) is the normal race after the leader exited; any other failure is
/// logged as a WARNING — a kill that silently did nothing is exactly the bug
/// class this module exists to remove.
#[cfg(unix)]
pub fn signal_group(pid: u32, sig: GroupSignal) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        tracing::warn!(
            target: CANCEL_TARGET,
            pid,
            signal = sig.as_str(),
            "process group signal skipped: unusable pid"
        );
        return false;
    }
    let signal = match sig {
        GroupSignal::Terminate => libc::SIGTERM,
        GroupSignal::Kill => libc::SIGKILL,
    };
    // SAFETY: kill(2) touches no process memory; a negative pid addresses the
    // group whose leader is `pid` (every child we spawn is a group leader).
    let rc = unsafe { libc::kill(-(pid as libc::pid_t), signal) };
    if rc == 0 {
        return true;
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() != Some(libc::ESRCH) {
        tracing::warn!(
            target: CANCEL_TARGET,
            pid,
            signal = sig.as_str(),
            error = %err,
            "process group signal failed"
        );
    }
    false
}

/// Non-Unix: no groups to signal (see the module docs).
#[cfg(not(unix))]
pub fn signal_group(_pid: u32, _sig: GroupSignal) -> bool {
    false
}

/// SIGKILL the whole process group led by `pid`.
pub fn kill_group(pid: u32) -> bool {
    signal_group(pid, GroupSignal::Kill)
}

/// Terminate `child` **and its whole process group**, then reap it.
///
/// This is the single termination point for timeouts and cancellations. The
/// direct `start_kill` stays as the Windows path and as a belt-and-braces when
/// the group signal is unavailable.
pub async fn terminate_group(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        kill_group(pid);
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[cfg(not(target_os = "windows"))]
mod tests {
    use super::*;
    use std::time::Duration;

    /// `kill -0` probe: true while `pid` exists.
    fn pid_alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    /// Poll until `pid` disappears (or the budget runs out).
    async fn wait_gone(pid: u32, budget: Duration) -> bool {
        let deadline = std::time::Instant::now() + budget;
        while std::time::Instant::now() < deadline {
            if !pid_alive(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        !pid_alive(pid)
    }

    /// Read `"<leader> <descendant>"` from a marker file written by the child.
    async fn read_pids(path: &std::path::Path) -> Option<(u32, u32)> {
        for _ in 0..100 {
            if let Ok(text) = std::fs::read_to_string(path) {
                let mut parts = text.split_whitespace();
                if let (Some(a), Some(b)) = (parts.next(), parts.next()) {
                    if let (Ok(a), Ok(b)) = (a.parse::<u32>(), b.parse::<u32>()) {
                        return Some((a, b));
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        None
    }

    /// Contract: a child spawned through this module leads its own process
    /// group, and terminating it kills the whole tree — not just the shell.
    ///
    /// Mutation guard (Scope 6, "去 killpg → 红"): drop the `kill_group` call in
    /// [`terminate_group`] and the descendant stays alive → red.
    #[tokio::test]
    async fn test_process_contract_process_group() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("pids");
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(format!(
            "sleep 60 & echo $$ $! > {}; wait",
            marker.display()
        ));

        let mut child = spawn_owned(&mut cmd).expect("spawn");

        let (leader, descendant) = read_pids(&marker)
            .await
            .expect("child should record its own and its descendant's pid");
        assert_eq!(
            child.id(),
            Some(leader),
            "the child must be the process-group leader we terminate"
        );
        assert!(
            pid_alive(descendant),
            "descendant {descendant} should be running before termination"
        );

        terminate_group(&mut child).await;

        assert!(
            wait_gone(leader, Duration::from_secs(5)).await,
            "leader {leader} still alive after terminate_group"
        );
        assert!(
            wait_gone(descendant, Duration::from_secs(5)).await,
            "descendant {descendant} survived terminate_group — killpg is missing (issue #165)"
        );
    }

    /// Contract: cancellation is data routed to an addressable handle.
    ///
    /// Mutation guard (Scope 6, "cancel 路径断裂 → 红"): make
    /// [`CancelHandle::cancel_with`] skip `token.cancel()` and the token
    /// assertions below go red.
    #[tokio::test]
    async fn cancel_registry_routes_by_key_and_scope() {
        let mut registry = CancelRegistry::new();
        let a = registry.register(CancelKind::Exec, Some("scope-a"));
        let b = registry.register(CancelKind::Exec, Some("scope-a"));
        let c = registry.register(CancelKind::BackgroundJob, Some("scope-b"));
        assert_eq!(registry.active(), 3);
        assert_eq!(registry.active_in_scope("scope-a"), 2);

        a.note_stage("draining-output");
        assert!(registry.cancel(a.key()), "explicit cancel must reach a");
        assert!(a.is_cancelled());
        assert!(
            !registry.cancel(a.key()),
            "a second cancel must report the record was already written"
        );
        assert!(!b.is_cancelled() && !c.is_cancelled());
        tokio::time::timeout(Duration::from_millis(10), a.cancelled())
            .await
            .expect("a cancelled handle awaits immediately");

        // Scope sweep reaches b only.
        assert_eq!(registry.cancel_scope("scope-a"), 1);
        assert!(b.is_cancelled());
        assert!(!c.is_cancelled());

        // The record carries who / what / how far.
        let events = registry.events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].key, a.key());
        assert_eq!(events[0].kind, CancelKind::Exec);
        assert_eq!(events[0].scope.as_deref(), Some("scope-a"));
        assert_eq!(events[0].reason, CancelReason::Explicit);
        assert_eq!(events[0].stage, "draining-output");
        assert!(events[0].at_ms > 0);
        assert_eq!(events[1].reason, CancelReason::Scope);

        assert!(registry.finish(c.key()));
        assert!(!registry.finish(c.key()));
        assert_eq!(registry.active(), 2);
    }

    /// Contract: a registration tracks the task's lifetime — dropping it
    /// deregisters, so the table never leaks stale routable entries.
    #[tokio::test]
    async fn task_registration_deregisters_on_drop() {
        let key = {
            let registration = TaskRegistration::spawn(CancelKind::Exec, Some("drop-scope"));
            let key = registration.key().to_string();
            assert!(key.starts_with("exec-"));
            assert_eq!(lock_tasks().active_in_scope("drop-scope"), 1);
            key
        };
        assert!(!lock_tasks().handles.contains_key(&key));
        assert_eq!(lock_tasks().active_in_scope("drop-scope"), 0);
    }

    /// Contract: a scope-wide cancel is a routing edge into the process-wide
    /// table — an edge that knows only the scope (execution / session root)
    /// ends the task without ever holding the handle.
    #[tokio::test]
    async fn global_scope_cancel_reaches_registered_task() {
        let scope = format!("goal-412-scope-{}", std::process::id());
        let registration = TaskRegistration::spawn(CancelKind::Exec, Some(&scope));
        let token = registration.token();
        assert!(!token.is_cancelled());

        assert_eq!(cancel_scope(&scope), 1);
        assert!(token.is_cancelled());
        assert!(registration.is_cancelled());
        assert!(!registration.stage().is_empty());

        drop(registration);
        assert_eq!(active_tasks(), lock_tasks().active());
    }

    /// Contract: cancellation lands a structured event on `recursive::tasks`
    /// (who / when / what / how far).
    ///
    /// Mutation guard (Scope 6): remove the `tracing::warn!` in
    /// [`CancelHandle::cancel_with`] and this test goes red.
    #[tracing_test::traced_test]
    #[tokio::test]
    async fn test_process_contract_cancel_observable() {
        let mut registry = CancelRegistry::new();
        let handle = registry.register(CancelKind::BackgroundJob, Some("sess-42"));
        handle.note_stage("reaping-process-group");

        assert!(registry.cancel(handle.key()));

        assert!(
            logs_contain("recursive::tasks"),
            "cancellation must land on the recursive::tasks target"
        );
        assert!(logs_contain("task cancelled"));
        assert!(logs_contain(handle.key()));
        assert!(logs_contain("reason=explicit"));
        assert!(logs_contain("stage=reaping-process-group"));
        assert!(logs_contain("sess-42"));

        let events = registry.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, "reaping-process-group");
    }
}
