//! `run_code` execution driver (issue #134): spawn a fresh Node process,
//! stream the protocol, and classify how the run ended.
//!
//! Execution contract (borrowed from DSH `ptc-runtime-node`):
//!
//! - a **fresh process** per run, with `env_clear` + only the executable
//!   search / system / temp paths;
//! - three budgets — wall clock, output bytes, heap MiB;
//! - sandbox availability is reported as a *fact*, separately from the run's
//!   own success or failure.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::error::Result;

use super::bindings::BindingTable;
use super::ledger::{truncate_to_budget, OutputLedger};
use super::protocol::{
    self, ExitOutcome, NodeEvent, ProgramError, RunCodeFailure, RunReport, SandboxFacts,
};

/// Default wall-clock budget (DSH's `ptc-runtime` default).
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// Hard ceiling for the wall-clock budget; a larger request is clamped.
pub const MAX_TIMEOUT_SECS: u64 = 600;
/// Default output budget — aligned with the tool layer's hard cap
/// (`shell::MAX_OUTPUT_BYTES_HARD_CAP`), so a program's observation can never
/// be larger than a shell command's.
pub const DEFAULT_OUTPUT_LIMIT_BYTES: usize = 2 * 1024 * 1024;
/// Default heap budget (512 MiB).
pub const DEFAULT_HEAP_LIMIT_MIB: usize = 512;
/// Env var that pins the runtime binary (tests / non-PATH installs).
pub const NODE_ENV_VAR: &str = "RECURSIVE_RUN_CODE_NODE";
/// How much stderr is captured (for failure classification + diagnostics).
const STDERR_CAPTURE_BYTES: usize = 64 * 1024;
/// Slack added to the output budget when bounding a single protocol line.
/// A line's JSON framing (and string escaping) outweighs its text, so a log
/// line whose text fits the budget must not be discarded as "oversized".
const PROTOCOL_LINE_SLACK_BYTES: usize = 64 * 1024;
/// Grace period for a killed process to be reaped.
const REAP_GRACE: Duration = Duration::from_secs(5);

const ENV_POLICY: &str = "cleared+exec-search/system/temp";

/// Goal 416: the filesystem policy enforced by the runtime permission model
/// when the probe finds support — `fs` writes confined to the run's scratch
/// dir, process spawning and worker threads denied, `fs` reads limited to the
/// read root (the workspace / process cwd) plus the scratch dir.
///
/// The wording is a **seat-belt statement, not a confinement claim**: Node's
/// own documentation is explicit that the permission model "does not protect
/// against malicious code" and "does not guarantee that users will not be
/// able to access the file system through other means, such as through the
/// `node:sqlite` module" (verified live on both macOS and Linux, node
/// 26.10.0: `DatabaseSync` creates and writes a database file outside every
/// allowlisted root). The fact names the bypass so neither the model nor the
/// operator reads `scratch-only` as a hostile-code security boundary;
/// containment of malicious code is the container / microVM transport tier,
/// where `run_code` is never mounted (see `mod.rs`).
pub const FS_POLICY_SANDBOXED: &str = "fs-write=scratch-only (seat-belt, not a \
     malicious-code boundary: node:sqlite bypasses fs grants); child_process=denied; \
     worker_threads=denied; cluster=denied; fs-read=read-root+scratch";

/// Goal 416: minimum free bytes the scratch volume must report before a run
/// is allowed to start. Below it the run is rejected *before* spawn with the
/// volume path and the measured free space in the failure detail.
pub const MIN_SCRATCH_FREE_BYTES: u64 = 64 * 1024 * 1024;

/// Headroom over the in-heap budget that the address-space limit adds for
/// everything outside the V8 old generation: native allocations, Buffers,
/// and — decisively on Linux — the runtime's own startup reservations.
/// Node 26.10 on Linux aborts during startup below ~875 MiB of address
/// space even with a 512 MiB heap (V8 `SegmentedTable` / cage reservations
/// plus glibc per-thread arena reserves), so this is sized to keep every
/// supported heap budget startable, not to tighten the heap cap.
#[cfg(any(unix, test))]
const RLIMIT_HEADROOM_MIB: usize = 512;

/// The embedded bootstrap (`src/tools/execution/run_code/bootstrap.js`).
pub const BOOTSTRAP_SOURCE: &str = include_str!("bootstrap.js");

/// Executes one tool call on behalf of a running program. Implemented for the
/// agent's tool registry so programmatic calls go through the same permission
/// pipeline as a model-issued call.
#[async_trait]
pub trait ToolInvoker: Send + Sync {
    async fn invoke(&self, tool: &str, args: Value) -> Result<String>;
}

/// The three execution budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunCodeLimits {
    pub timeout: Duration,
    pub output_limit_bytes: usize,
    pub heap_limit_mib: usize,
}

impl Default for RunCodeLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            output_limit_bytes: DEFAULT_OUTPUT_LIMIT_BYTES,
            heap_limit_mib: DEFAULT_HEAP_LIMIT_MIB,
        }
    }
}

impl RunCodeLimits {
    /// Clamp the wall-clock budget to [`MAX_TIMEOUT_SECS`]; the output and heap
    /// budgets are taken as declared (tests use small ones deliberately).
    pub fn clamped(self) -> Self {
        Self {
            timeout: self.timeout.min(Duration::from_secs(MAX_TIMEOUT_SECS)),
            ..self
        }
    }
}

/// Everything one run needs.
pub struct RunProgramRequest {
    pub source: String,
    pub bindings: BindingTable,
    pub invoker: Arc<dyn ToolInvoker>,
    pub limits: RunCodeLimits,
    /// Explicit runtime binary; `None` locates `node` on `PATH`.
    pub node_bin: Option<PathBuf>,
    /// Host-side cancellation (Ctrl-C / session teardown).
    pub abort: Option<CancellationToken>,
    /// Goal 416: the directory the program may read (the workspace root).
    /// `None` resolves to the host process's current directory at spawn time
    /// — the same root the runtime process used to inherit.
    pub read_root: Option<PathBuf>,
    /// Goal 416: scratch-volume free-space floor for this run (tests force a
    /// rejection without needing a full disk). `None` uses
    /// [`MIN_SCRATCH_FREE_BYTES`].
    pub min_free_bytes: Option<u64>,
}

/// Resolve the runtime binary: `RECURSIVE_RUN_CODE_NODE` wins (and, when set
/// but missing, means "unavailable" rather than a silent PATH fallback).
pub fn locate_node() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os(NODE_ENV_VAR) {
        let path = PathBuf::from(raw);
        return path.is_file().then_some(path);
    }
    let path = std::env::var_os("PATH")?;
    let exe = if cfg!(windows) { "node.exe" } else { "node" };
    std::env::split_paths(&path)
        .map(|dir| dir.join(exe))
        .find(|candidate| candidate.is_file())
}

/// The environment a fresh run inherits: the executable search path plus the
/// system / temp paths the runtime needs to start. Everything else (all
/// credentials in particular) is dropped by `env_clear`.
fn child_env() -> Vec<(OsString, OsString)> {
    let keys: &[&str] = if cfg!(windows) {
        &["PATH", "SystemRoot", "windir", "PATHEXT", "TEMP", "TMP"]
    } else {
        &["PATH", "TMPDIR"]
    };
    let mut vars: Vec<(OsString, OsString)> = Vec::new();
    for key in keys {
        if let Some(value) = std::env::var_os(key) {
            vars.push((OsString::from(key), value));
        }
    }
    vars
}

/// How one [`read_capped_line`] call ended.
enum CappedLine {
    /// A complete line (without its trailing newline) is in the buffer.
    Line,
    /// The line exceeded `cap`; the buffer holds its first `cap` bytes and the
    /// rest was consumed and dropped.
    Overflow,
    /// End of stream.
    Eof,
}

/// Read one newline-terminated line into `out`, never buffering more than
/// `cap` bytes of it.
///
/// `BufReader::lines()` / `next_line()` buffer a whole line before the caller
/// sees it, so a program writing one newline-free blob would make the *agent*
/// process allocate all of it. This reader keeps the host's memory bounded by
/// the output budget: past `cap` the remainder of the line is consumed and
/// dropped, and the caller marks the run truncated.
async fn read_capped_line<R>(
    reader: &mut R,
    out: &mut Vec<u8>,
    cap: usize,
) -> std::io::Result<CappedLine>
where
    R: AsyncBufRead + Unpin,
{
    out.clear();
    let mut overflowed = false;
    loop {
        let (consume, saw_newline) = {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                return Ok(match (out.is_empty(), overflowed) {
                    (true, false) => CappedLine::Eof,
                    (_, false) => CappedLine::Line,
                    (_, true) => CappedLine::Overflow,
                });
            }
            let newline = available.iter().position(|&b| b == b'\n');
            let take = newline.map_or(available.len(), |idx| idx + 1);
            if !overflowed {
                if out.len() + take > cap {
                    let room = cap - out.len();
                    out.extend_from_slice(&available[..room]);
                    overflowed = true;
                } else {
                    out.extend_from_slice(&available[..take]);
                }
            }
            (take, newline.is_some())
        };
        reader.consume(consume);
        if saw_newline {
            return Ok(if overflowed {
                CappedLine::Overflow
            } else {
                CappedLine::Line
            });
        }
    }
}

async fn read_stderr(stderr: tokio::process::ChildStderr) -> String {
    let mut reader = BufReader::new(stderr);
    let mut captured: Vec<u8> = Vec::new();
    let mut line: Vec<u8> = Vec::new();
    loop {
        match read_capped_line(&mut reader, &mut line, STDERR_CAPTURE_BYTES).await {
            Ok(CappedLine::Eof) | Err(_) => break,
            Ok(CappedLine::Line) | Ok(CappedLine::Overflow) => {
                let room = STDERR_CAPTURE_BYTES.saturating_sub(captured.len());
                let take = line.len().min(room);
                captured.extend_from_slice(&line[..take]);
            }
        }
    }
    String::from_utf8_lossy(&captured).into_owned()
}

async fn wait_abort(token: Option<CancellationToken>) {
    match token {
        Some(token) => token.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}

/// A lost call-result must stay observable (issue #218): every write failure
/// warns, and the first one is kept in `slot` so the run can be classified
/// `protocol` with the call id in its detail once the write tasks are joined.
async fn record_stdin_write_failure(slot: &Mutex<Option<String>>, message: String) {
    tracing::warn!("{message}");
    let mut guard = slot.lock().await;
    if guard.is_none() {
        *guard = Some(message);
    }
}

fn unavailable_report(sandbox: SandboxFacts, started: Instant, detail: &str) -> RunReport {
    RunReport {
        failure: Some(RunCodeFailure::SandboxUnavailable),
        exit_code: None,
        calls: 0,
        duration_ms: started.elapsed().as_millis(),
        output: String::new(),
        output_bytes: 0,
        output_truncated: false,
        value: None,
        detail: Some(detail.to_string()),
        sandbox,
    }
}

// ---------------------------------------------------------------------------
// Goal 416 — local-sandbox security baseline
//
// The program is an opaque blob: without a runtime-enforced permission model
// it is a full host process (more capable than Bash, with none of Bash's
// review). The baseline below makes `run_code` fail loudly instead of
// silently degrading:
//
// - the runtime is probed for a permission model (the flag name moved between
//   Node lines, so support is *probed*, never assumed from a single flag);
// - no support (or a failed probe) → every call reports `sandbox-unavailable`
//   with the concrete reason — never a fall-back to an unsandboxed run;
// - the process writes only into its own mkdtemp scratch dir (its spawn cwd),
//   and the scratch volume's free space is checked before spawn;
// - the address space is bounded via `pre_exec` where the kernel enforces
//   RLIMIT_AS, with the enforcement completeness reported as a fact.
// ---------------------------------------------------------------------------

/// The permission flag a runtime was probed with, and where it is cached.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PermissionProbe {
    /// The accepted flag (`--permission` or the older
    /// `--experimental-permission`).
    flag: &'static str,
}

/// Probe verdicts cached per runtime binary. The probe spawns a process
/// (~70 ms for a real `node`), so a session's many runs share the verdict;
/// the cache key is the resolved binary path, so different shims / runtimes
/// never collide.
fn probe_cache() -> &'static std::sync::Mutex<std::collections::HashMap<PathBuf, ProbeVerdict>> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<PathBuf, ProbeVerdict>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Why the probe concluded a runtime has no usable permission model.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProbeVerdict {
    Supported(PermissionProbe),
    Unsupported(String),
}

/// Whether `flag` is accepted by `node`. A runtime that does not know the
/// flag exits non-zero ("bad option") without side effects; a supported one
/// runs the trivial `-e` program and exits 0.
async fn flag_accepted(node: &Path, flag: &str) -> std::io::Result<bool> {
    let probe = Command::new(node)
        .arg(flag)
        .arg("-e")
        .arg("process.exit(0)")
        .env_clear()
        .envs(child_env())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(Duration::from_secs(10), probe).await;
    match output {
        // Probe timed out: the runtime is wedged, not "supported" — a fail
        // closed verdict, never a silent unsandboxed run.
        Err(_) => Ok(false),
        Ok(Ok(output)) => Ok(output.status.success()),
        Ok(Err(err)) => Err(err),
    }
}

/// Probe `node` for a runtime-enforced permission model. Flag names moved
/// between Node lines (`--experimental-permission` → `--permission`), so the
/// candidates are tried in order and the accepted one is used — that is
/// compatibility, not a downgrade. The red line is the third case: no
/// candidate accepted (or the probe itself failed) means the run is rejected.
async fn probe_permission_model(node: &Path) -> ProbeVerdict {
    let cached = probe_cache()
        .lock()
        .ok()
        .and_then(|cache| cache.get(node).cloned());
    if let Some(verdict) = cached {
        return verdict;
    }
    const CANDIDATES: [&str; 2] = ["--permission", "--experimental-permission"];
    let mut last_error: Option<String> = None;
    let mut rejected: Vec<String> = Vec::new();
    let mut verdict = None;
    for flag in CANDIDATES {
        match flag_accepted(node, flag).await {
            Ok(true) => {
                verdict = Some(ProbeVerdict::Supported(PermissionProbe { flag }));
                break;
            }
            Ok(false) => {
                rejected.push(format!("{node:?} does not accept {flag}"));
            }
            Err(err) => {
                last_error = Some(format!(
                    "launching the permission probe for {flag} failed: {err}"
                ));
            }
        }
    }
    let verdict = verdict.unwrap_or_else(|| {
        if !rejected.is_empty() {
            ProbeVerdict::Unsupported(rejected.join("; "))
        } else {
            ProbeVerdict::Unsupported(
                last_error
                    .unwrap_or_else(|| "the permission probe produced no verdict".to_string()),
            )
        }
    });
    if let Ok(mut cache) = probe_cache().lock() {
        cache.insert(node.to_path_buf(), verdict.clone());
    }
    verdict
}

/// Create the run's private scratch directory under the system temp dir.
/// `create_dir` fails on an existing path, so the pid+nanos suffix gives the
/// exclusivity `mkdtemp` would.
fn create_scratch_dir() -> std::io::Result<PathBuf> {
    let base = std::env::temp_dir();
    let pid = std::process::id();
    for _ in 0..64 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let dir = base.join(format!("recursive-run-code-{pid}-{nanos}"));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(std::io::Error::other(
        "could not create a unique scratch directory after 64 attempts",
    ))
}

/// Best-effort scratch cleanup. A leftover is never silent: the operator is
/// told what survived and why (降级可观测).
fn cleanup_scratch(dir: &Path) {
    if let Err(err) = std::fs::remove_dir_all(dir) {
        if err.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                path = %dir.display(),
                error = %err,
                "run_code: scratch directory cleanup failed; files may remain"
            );
        }
    }
}

/// Free bytes available to unprivileged users on the volume holding `path`
/// (statfs `f_bsize × f_bavail`). `None` when the platform offers no probe.
#[cfg(unix)]
fn scratch_volume_free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt as _;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statfs` writes `count` bytes into the out struct and reads the
    // NUL-terminated path; both are valid for the duration of the call.
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statfs(c_path.as_ptr(), &mut buf) };
    if rc != 0 {
        return None;
    }
    Some(buf.f_bsize as u64 * buf.f_bavail)
}

/// Non-unix: no disk probe (the fact string says so rather than claiming one).
#[cfg(not(unix))]
fn scratch_volume_free_bytes(_path: &Path) -> Option<u64> {
    None
}

/// The address-space limit handed to `pre_exec`: the in-heap budget plus the
/// out-of-heap headroom, floored so a tiny test budget cannot strand the
/// runtime before its own startup allocations.
///
/// The headroom is not slack — on Linux, Node's startup reserves large
/// address ranges that a tight `RLIMIT_AS` converts into a hard startup
/// failure: V8's `SegmentedTable` / pointer-compression cage reservations plus
/// glibc's per-arena reservations. Measured on Linux/aarch64, Node 26.10,
/// `--max-old-space-size=512`: startup aborts below ~875 MiB of address
/// space, and the same process with a 2 GiB limit runs the full 512 MiB heap
/// fine. [`RLIMIT_HEADROOM_MIB`] keeps every supported heap budget on the
/// safe side of that line while still capping the process well below
/// "unbounded"; the enforced value is reported in the sandbox facts.
///
/// CI (ubuntu 24 x86_64, node 26) measured: a 528 MiB ceiling SIGKILLs node
/// at startup (exit code lost — the kernel kills before V8 can classify its
/// own heap abort), so the floor is 1 GiB: comfortably above every measured
/// startup need while still bounding the process.
#[cfg(any(unix, test))]
fn address_space_limit_bytes(heap_limit_mib: usize) -> u64 {
    heap_limit_mib.saturating_add(RLIMIT_HEADROOM_MIB).max(1024) as u64 * 1024 * 1024
}

/// Install the RLIMIT_AS guard in the forked child. Returns the enforcement
/// statement the *parent* should report for this platform: on Linux the limit
/// is enforced and a failed `setrlimit` aborts the spawn (the error surfaces
/// through `spawn()`); on kernels that refuse to lower `RLIMIT_AS` (macOS)
/// the failure is the kernel's documented stance, so the run proceeds with
/// the non-enforcement recorded — never claimed as enforced.
#[cfg(unix)]
fn address_space_guard(hard_limit_bytes: u64) -> impl FnMut() -> std::io::Result<()> {
    move || {
        let new_limit = libc::rlimit {
            rlim_cur: hard_limit_bytes,
            rlim_max: libc::RLIM_INFINITY,
        };
        let rc = unsafe { libc::setrlimit(libc::RLIMIT_AS, &new_limit) };
        if rc == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        if matches!(
            err.raw_os_error(),
            Some(libc::EINVAL) | Some(libc::EPERM) | Some(libc::ENOTSUP)
        ) {
            // The kernel refuses to lower RLIMIT_AS (macOS: EINVAL by
            // design). Leaving the limit untouched is that kernel's
            // documented non-enforcement — the parent records it in the
            // sandbox facts rather than claiming a cap that does not exist.
            return Ok(());
        }
        Err(err)
    }
}

/// The enforcement statement matching [`address_space_guard`] for a spawn
/// that succeeded: Linux applied the limit (a failure would have failed the
/// spawn); kernels that cannot lower RLIMIT_AS report exactly that.
#[cfg(unix)]
fn address_space_guard_fact(heap_limit_mib: usize) -> String {
    if cfg!(target_os = "linux") {
        format!(
            "rlimit_as=enforced({}MiB=heap+{}MiB headroom)",
            heap_limit_mib + RLIMIT_HEADROOM_MIB,
            RLIMIT_HEADROOM_MIB
        )
    } else {
        "rlimit_as=not-enforced-by-kernel; in-heap bounded by the heap limit".to_string()
    }
}

#[cfg(not(unix))]
fn address_space_guard_fact(_heap_limit_mib: usize) -> String {
    "rlimit_as=not-probed(platform); in-heap bounded by the heap limit".to_string()
}

/// Run one model-authored program. Never fails: every outcome — including an
/// unavailable sandbox — is reported as a [`RunReport`].
pub async fn run_program(req: RunProgramRequest) -> RunReport {
    let RunProgramRequest {
        source,
        bindings,
        invoker,
        limits,
        node_bin,
        abort,
        read_root,
        min_free_bytes,
    } = req;
    let limits = limits.clamped();
    let started = Instant::now();
    let mut sandbox = SandboxFacts {
        runtime: None,
        env_policy: ENV_POLICY.to_string(),
        heap_limit_mib: limits.heap_limit_mib,
        output_limit_bytes: limits.output_limit_bytes,
        timeout_secs: limits.timeout.as_secs(),
        fs_policy: String::new(),
        permission_flag: None,
        scratch_dir: None,
        resource_guards: String::new(),
    };

    let Some(node) = node_bin.or_else(locate_node) else {
        return unavailable_report(
            sandbox,
            started,
            &format!("no JavaScript runtime found (install `node` or set {NODE_ENV_VAR})"),
        );
    };
    sandbox.runtime = Some(node.display().to_string());

    // Goal 416: no runtime-enforced permission model means no sandbox — the
    // run is rejected with the concrete probe reason instead of degrading to
    // an unsandboxed full-host-process execution.
    let permission = probe_permission_model(&node).await;
    let permission_flag = match &permission {
        ProbeVerdict::Supported(probe) => probe.flag,
        ProbeVerdict::Unsupported(reason) => {
            return unavailable_report(
                sandbox,
                started,
                &format!(
                    "runtime has no usable permission model, refusing to run unsandboxed: \
                     {reason}"
                ),
            );
        }
    };
    sandbox.permission_flag = Some(permission_flag.to_string());
    sandbox.fs_policy = FS_POLICY_SANDBOXED.to_string();
    sandbox.resource_guards = address_space_guard_fact(limits.heap_limit_mib);

    // Goal 416: the program's only writable location is this per-run scratch
    // dir — it is also the spawn cwd, so a relative path can never escape it.
    let scratch = match create_scratch_dir() {
        Ok(dir) => dir,
        Err(err) => {
            return unavailable_report(
                sandbox,
                started,
                &format!("could not create the run's scratch directory: {err}"),
            );
        }
    };
    sandbox.scratch_dir = Some(scratch.display().to_string());

    // Goal 416: a full disk is a pre-spawn rejection with the volume and the
    // measured free space, not a mid-run mystery.
    let floor = min_free_bytes.unwrap_or(MIN_SCRATCH_FREE_BYTES);
    let free = scratch_volume_free_bytes(&scratch);
    if let Some(free_bytes) = free {
        if free_bytes < floor {
            let detail = format!(
                "scratch volume has insufficient free space: {} bytes free on volume of {}, \
                 refusing to start (floor {floor} bytes)",
                free_bytes,
                scratch.display(),
            );
            cleanup_scratch(&scratch);
            return unavailable_report(sandbox, started, &detail);
        }
    } else {
        tracing::warn!(
            path = %scratch.display(),
            "run_code: scratch-volume free space could not be probed; proceeding without a \
             disk check"
        );
    }

    let read_root = read_root
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| scratch.clone());

    let mut cmd = Command::new(&node);
    cmd.arg(permission_flag);
    // Linux self-inspection: `/proc/self/limits` is how the child observes
    // its own address-space limit (the live regression asserts the guard is
    // what the facts claim). Without an explicit grant the permission model
    // denies the read with `ERR_ACCESS_DENIED` — verified, there is no
    // /proc carve-out.
    #[cfg(target_os = "linux")]
    cmd.arg("--allow-fs-read=/proc");
    for dir in [&read_root, &scratch] {
        if dir.exists() {
            cmd.arg(format!("--allow-fs-read={}", dir.display()));
        } else {
            // A non-existent grant root would be accepted by Node as an
            // exact-path entry (no trailing wildcard), so the fact line's
            // `read-root` claim would quietly narrow. The read root is the
            // host process's cwd or the pinned workspace root — it exists
            // when the host is running — so this is a loud impossibility,
            // not a branch the model can steer into.
            tracing::warn!(
                path = %dir.display(),
                "run_code: read root missing at spawn; fs-read grant skipped"
            );
        }
    }
    // The scratch dir twice: the absolute spelling (absolute-path writes)
    // and the cwd-relative `.` spelling — the permission model resolves a
    // *relative* write against cwd and checks it against the allowlist
    // entries as given, so `.` is what makes `writeFileSync("x")` land in
    // the scratch dir instead of being denied outright.
    cmd.arg(format!("--allow-fs-write={}", scratch.display()));
    cmd.arg("--allow-fs-write=.");
    cmd.arg(format!("--max-old-space-size={}", limits.heap_limit_mib));
    cmd.arg("-e");
    cmd.arg(BOOTSTRAP_SOURCE);
    cmd.current_dir(&scratch);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    cmd.env_clear();
    for (key, value) in child_env() {
        cmd.env(key, value);
    }
    // Goal 412: the runtime leads its own process group so a timed-out program
    // that spawned its own children is ended as a tree, not just at the root.
    crate::process::attach_group(&mut cmd);

    // Goal 416: bound the address space in the forked child (before exec), so
    // out-of-heap allocations (Buffers / native) hit a kernel-level ceiling
    // too. On a kernel that enforces RLIMIT_AS a failed setrlimit fails the
    // spawn — surfaced below as `sandbox-unavailable`, never silently dropped.
    // The limit value carries generous startup headroom: Node's own startup
    // reservations (V8 pointer-compression cage, glibc arenas) abort the
    // process below ~875 MiB of address space with a 512 MiB heap on Linux,
    // so the heap+headroom formula is the floor that keeps every supported
    // heap budget startable, not a tightening of the heap cap (which stays
    // `--max-old-space-size`).
    #[cfg(unix)]
    {
        let hard_limit = address_space_limit_bytes(limits.heap_limit_mib);
        unsafe {
            cmd.pre_exec(move || address_space_guard(hard_limit)());
        }
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            cleanup_scratch(&scratch);
            let detail = format!("failed to start runtime {}: {err}", node.display());
            return unavailable_report(sandbox, started, &detail);
        }
    };

    let program_line = json!({
        "t": "program",
        "source": source,
        "bindings": bindings.names(),
    })
    .to_string();

    let mut ledger = OutputLedger::new(limits.output_limit_bytes);
    let mut calls = 0usize;
    let mut value: Option<String> = None;
    let mut program_error: Option<ProgramError> = None;
    let mut protocol_error: Option<String> = None;
    let mut timed_out = false;
    let mut aborted = false;

    let stdin = Arc::new(Mutex::new(child.stdin.take()));
    let stdin_write_error = Arc::new(Mutex::new(None::<String>));
    let mut result_write_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    {
        let mut guard = stdin.lock().await;
        if let Some(handle) = guard.as_mut() {
            if handle
                .write_all(format!("{program_line}\n").as_bytes())
                .await
                .is_err()
            {
                protocol_error = Some("failed to send the program to the runtime".to_string());
            } else if handle.flush().await.is_err() {
                protocol_error = Some("failed to flush the program to the runtime".to_string());
            }
        } else {
            protocol_error = Some("runtime stdin was not available".to_string());
        }
    }

    let stderr_task = child
        .stderr
        .take()
        .map(|stderr| tokio::spawn(read_stderr(stderr)));
    let mut stdout = child.stdout.take().map(BufReader::new);
    // One line is buffered at most up to `line_cap`; a bigger single line is
    // dropped by the read (see `read_capped_line`) so the agent's memory stays
    // bounded by the run's output budget.
    let line_cap = limits
        .output_limit_bytes
        .saturating_add(PROTOCOL_LINE_SLACK_BYTES);
    let mut line: Vec<u8> = Vec::new();

    let deadline = tokio::time::sleep(limits.timeout);
    tokio::pin!(deadline);
    let abort_future = wait_abort(abort);
    tokio::pin!(abort_future);

    if let Some(reader) = stdout.as_mut() {
        loop {
            tokio::select! {
                _ = &mut deadline => {
                    timed_out = true;
                    break;
                }
                _ = &mut abort_future => {
                    aborted = true;
                    break;
                }
                next = read_capped_line(reader, &mut line, line_cap) => {
                    match next {
                        Ok(CappedLine::Eof) => break,
                        Ok(CappedLine::Overflow) => {
                            // A single protocol line overran the output budget:
                            // its bytes were dropped rather than buffered, and
                            // the run is reported as `output-limit`.
                            ledger.mark_truncated();
                            continue;
                        }
                        Ok(CappedLine::Line) => {
                            let raw = String::from_utf8_lossy(&line);
                            let raw = raw.trim_end_matches('\n').trim_end_matches('\r');
                            match protocol::parse_event(raw) {
                                Ok(NodeEvent::Log { text, .. }) => {
                                    ledger.append(&text);
                                    ledger.append("\n");
                                }
                                Ok(NodeEvent::Result { value: produced }) => {
                                    value = produced;
                                }
                                Ok(NodeEvent::Error { name, message, .. }) => {
                                    program_error = Some(ProgramError { name, message });
                                }
                                Ok(NodeEvent::Call { id, tool, args }) => {
                                    calls += 1;
                                    let invoker = invoker.clone();
                                    let stdin = stdin.clone();
                                    let stdin_write_error = stdin_write_error.clone();
                                    result_write_tasks.push(tokio::spawn(async move {
                                        let message = match invoker.invoke(&tool, args).await {
                                            Ok(value) => json!({
                                                "t": "call-result", "id": id, "ok": true, "value": value
                                            }),
                                            Err(err) => json!({
                                                "t": "call-result", "id": id, "ok": false,
                                                "error": err.to_string()
                                            }),
                                        };
                                        let line = format!("{message}\n");
                                        let mut guard = stdin.lock().await;
                                        match guard.as_mut() {
                                            Some(handle) => {
                                                if let Err(err) =
                                                    handle.write_all(line.as_bytes()).await
                                                {
                                                    record_stdin_write_failure(
                                                        &stdin_write_error,
                                                        format!(
                                                            "writing call-result {id} to \
                                                             runtime stdin failed: {err}"
                                                        ),
                                                    )
                                                    .await;
                                                } else if let Err(err) = handle.flush().await {
                                                    record_stdin_write_failure(
                                                        &stdin_write_error,
                                                        format!(
                                                            "flushing call-result {id} to \
                                                             runtime stdin failed: {err}"
                                                        ),
                                                    )
                                                    .await;
                                                }
                                            }
                                            None => {
                                                record_stdin_write_failure(
                                                    &stdin_write_error,
                                                    format!(
                                                        "runtime stdin was not available for \
                                                         call-result {id}"
                                                    ),
                                                )
                                                .await;
                                            }
                                        }
                                    }));
                                }
                                Err(err) => {
                                    protocol_error = Some(format!(
                                        "runtime stdout line is not a protocol message: {err}"
                                    ));
                                }
                            }
                        }
                        Err(err) => {
                            protocol_error =
                                Some(format!("reading runtime stdout failed: {err}"));
                            break;
                        }
                    }
                }
            }
        }
    }

    if timed_out || aborted {
        // Goal 412: kill the group the runtime leads, then the root process —
        // a program that spawned children must not leave them behind on a
        // timeout (issue #165 class).
        if let Some(pid) = child.id() {
            crate::process::kill_group(pid);
        }
        let _ = child.start_kill();
    }
    let status = match tokio::time::timeout(REAP_GRACE, child.wait()).await {
        Ok(Ok(status)) => Some(status),
        Ok(Err(_)) | Err(_) => None,
    };

    // The stderr join is bounded for the same reason `child.wait` is: a
    // program can leave a detached process holding the inherited stderr pipe,
    // and an unbounded `await` would then let the tool call — and the agent
    // turn — outlast the wall-clock budget it was promised (issue #134
    // review). After the grace the reader is aborted: the runtime is already
    // dead, so whatever is left on that pipe is not ours to wait for.
    let stderr = match stderr_task {
        Some(mut task) => match tokio::time::timeout(REAP_GRACE, &mut task).await {
            Ok(Ok(captured)) => captured,
            Ok(Err(_)) => String::new(),
            Err(_) => {
                task.abort();
                String::new()
            }
        },
        None => String::new(),
    };

    // Issue #218: the runtime is reaped by now, so outstanding call-result
    // writes fail fast; the shared grace only bounds an `invoke` still in
    // flight. A recorded write failure then reclassifies the run as
    // `protocol` — unless a stdout-side protocol error already explains it.
    let write_grace = tokio::time::sleep(REAP_GRACE);
    tokio::pin!(write_grace);
    for mut task in result_write_tasks {
        tokio::select! {
            _ = &mut task => {}
            _ = &mut write_grace => task.abort(),
        }
    }
    if protocol_error.is_none() {
        if let Some(message) = stdin_write_error.lock().await.take() {
            protocol_error = Some(message);
        }
    }

    // The completion value shares the run's output budget with the log lines:
    // `render` prints both, so letting the value through unbounded (or even at
    // the full line cap) would ship an observation up to twice the documented
    // ceiling — and report `truncated` while still delivering the whole value.
    let (value, value_truncated) = match value {
        Some(produced) => {
            let (kept, dropped) = truncate_to_budget(&produced, ledger.remaining());
            (Some(kept.to_string()), dropped)
        }
        None => (None, false),
    };

    let exit_code = status.as_ref().and_then(|status| status.code());
    let killed_by_signal = status
        .as_ref()
        .is_some_and(|status| status.code().is_none());
    // The run is truncated if either half of the observation lost bytes, so a
    // value that did not fit is reported as `output-limit` exactly like an
    // over-long log line.
    let output_truncated = ledger.is_truncated() || value_truncated;
    let outcome = ExitOutcome {
        timed_out,
        aborted,
        exit_code,
        killed_by_signal,
        stderr: stderr.clone(),
        program_error: program_error.clone(),
        protocol_error: protocol_error.clone(),
        output_truncated,
    };
    let failure = outcome.classify();
    let output = ledger.text();
    let output_bytes = ledger.len() + value.as_ref().map_or(0, String::len);
    let duration_ms = started.elapsed().as_millis();

    let detail = failure.map(|failure| {
        let ctx = FailureContext {
            program_error: program_error.as_ref(),
            protocol_error: protocol_error.as_deref(),
            exit_code,
            stderr: &stderr,
            output_bytes,
        };
        describe_failure(failure, &limits, &ctx)
    });

    // Goal 416: the scratch dir dies with the run — success, timeout, abort,
    // crash — every path funnels through here. A failed cleanup warns (see
    // `cleanup_scratch`) instead of stranding files silently.
    cleanup_scratch(&scratch);

    RunReport {
        failure,
        exit_code,
        calls,
        duration_ms,
        output,
        output_bytes,
        output_truncated,
        value,
        detail,
        sandbox,
    }
}

/// The run-level facts a failure description is derived from.
struct FailureContext<'a> {
    program_error: Option<&'a ProgramError>,
    protocol_error: Option<&'a str>,
    exit_code: Option<i32>,
    stderr: &'a str,
    output_bytes: usize,
}

/// Render the human-readable reason a run failed.
fn describe_failure(
    failure: RunCodeFailure,
    limits: &RunCodeLimits,
    ctx: &FailureContext<'_>,
) -> String {
    match failure {
        RunCodeFailure::Exception => ctx
            .program_error
            .map(|err| format!("program threw {}: {}", err.name, err.message))
            .unwrap_or_else(|| "program threw".to_string()),
        RunCodeFailure::Timeout => format!(
            "exceeded the {}s wall-clock budget",
            limits.timeout.as_secs()
        ),
        RunCodeFailure::Abort => "cancelled by the host".to_string(),
        RunCodeFailure::InvalidOutput => ctx
            .program_error
            .map(|err| format!("completion value could not be serialized: {}", err.message))
            .unwrap_or_else(|| "completion value could not be serialized".to_string()),
        RunCodeFailure::OutputLimit => format!(
            "output budget of {} bytes exhausted; retained the first {} bytes",
            limits.output_limit_bytes, ctx.output_bytes
        ),
        RunCodeFailure::Protocol => ctx
            .protocol_error
            .unwrap_or("runtime emitted an unparsable message")
            .to_string(),
        RunCodeFailure::SandboxUnavailable => "the execution sandbox was unavailable".to_string(),
        RunCodeFailure::HeapLimit => {
            format!("exceeded the {} MiB heap budget", limits.heap_limit_mib)
        }
        RunCodeFailure::WorkerExit => {
            let code = ctx
                .exit_code
                .map_or_else(|| "signal".to_string(), |c| c.to_string());
            let first_stderr = ctx.stderr.lines().next().unwrap_or("");
            if first_stderr.is_empty() {
                format!("runtime exited with {code} before the program finished")
            } else {
                format!("runtime exited with {code} before the program finished: {first_stderr}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_budgets() {
        let limits = RunCodeLimits::default();
        assert_eq!(limits.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        assert_eq!(limits.output_limit_bytes, DEFAULT_OUTPUT_LIMIT_BYTES);
        assert_eq!(limits.heap_limit_mib, DEFAULT_HEAP_LIMIT_MIB);
        assert_eq!(DEFAULT_TIMEOUT_SECS, 120);
        assert_eq!(MAX_TIMEOUT_SECS, 600);
        assert_eq!(DEFAULT_OUTPUT_LIMIT_BYTES, 2 * 1024 * 1024);
        assert_eq!(DEFAULT_HEAP_LIMIT_MIB, 512);
    }

    /// The output budget must not exceed the tool layer's hard cap, or a run
    /// could feed the model a larger observation than any other tool.
    #[test]
    fn default_output_budget_matches_the_tool_layer_cap() {
        assert_eq!(
            DEFAULT_OUTPUT_LIMIT_BYTES,
            crate::tools::shell::MAX_OUTPUT_BYTES_HARD_CAP
        );
    }

    /// The reader buffers at most `cap` bytes of a line: a newline-free blob
    /// is dropped, not held in memory.
    #[tokio::test]
    async fn read_capped_line_bounds_a_newline_free_blob() {
        let mut reader = BufReader::new(std::io::Cursor::new(vec![b'x'; 10_000]));
        let mut out = Vec::new();
        let outcome = read_capped_line(&mut reader, &mut out, 64).await.unwrap();
        assert!(matches!(outcome, CappedLine::Overflow));
        assert_eq!(out.len(), 64, "must keep only the capped prefix");
    }

    #[tokio::test]
    async fn read_capped_line_preserves_complete_lines() {
        let mut reader = BufReader::new(std::io::Cursor::new(b"one\ntwo\n".to_vec()));
        let mut out = Vec::new();
        assert!(matches!(
            read_capped_line(&mut reader, &mut out, 64).await.unwrap(),
            CappedLine::Line
        ));
        assert_eq!(out, b"one\n");
        assert!(matches!(
            read_capped_line(&mut reader, &mut out, 64).await.unwrap(),
            CappedLine::Line
        ));
        assert_eq!(out, b"two\n");
        assert!(matches!(
            read_capped_line(&mut reader, &mut out, 64).await.unwrap(),
            CappedLine::Eof
        ));
    }

    /// An over-long line in the middle of a stream must not swallow the lines
    /// that follow it.
    #[tokio::test]
    async fn read_capped_line_resumes_after_an_overflow() {
        let mut payload = vec![b'x'; 500];
        payload.push(b'\n');
        payload.extend_from_slice(b"after\n");
        let mut reader = BufReader::new(std::io::Cursor::new(payload));
        let mut out = Vec::new();
        assert!(matches!(
            read_capped_line(&mut reader, &mut out, 64).await.unwrap(),
            CappedLine::Overflow
        ));
        assert!(matches!(
            read_capped_line(&mut reader, &mut out, 64).await.unwrap(),
            CappedLine::Line
        ));
        assert_eq!(out, b"after\n");
    }

    #[test]
    fn clamped_caps_only_the_wall_clock_budget() {
        let over = RunCodeLimits {
            timeout: Duration::from_secs(99_999),
            ..RunCodeLimits::default()
        }
        .clamped();
        assert_eq!(over.timeout, Duration::from_secs(MAX_TIMEOUT_SECS));

        let under = RunCodeLimits {
            timeout: Duration::from_secs(5),
            ..RunCodeLimits::default()
        }
        .clamped();
        assert_eq!(under.timeout, Duration::from_secs(5));
    }

    #[test]
    fn child_env_carries_only_the_allowed_paths() {
        let vars = child_env();
        let allowed = [
            "PATH",
            "SystemRoot",
            "windir",
            "PATHEXT",
            "TEMP",
            "TMP",
            "TMPDIR",
        ];
        for (key, _) in &vars {
            let key = key.to_string_lossy();
            assert!(
                allowed.contains(&key.as_ref()),
                "unexpected env key leaked into the fresh process: {key}"
            );
        }
    }

    /// Every failure variant has a distinct, human-readable reason.
    #[test]
    fn every_failure_variant_is_described() {
        let limits = RunCodeLimits::default();
        let error = ProgramError {
            name: "TypeError".into(),
            message: "boom".into(),
        };
        let program_error = Some(&error);
        let ctx = FailureContext {
            program_error,
            protocol_error: Some("garbage line"),
            exit_code: Some(3),
            stderr: "Exploded\nmore",
            output_bytes: 11,
        };
        assert!(describe_failure(RunCodeFailure::Exception, &limits, &ctx).contains("TypeError"));
        assert!(describe_failure(RunCodeFailure::Timeout, &limits, &ctx).contains("120s"));
        assert!(describe_failure(RunCodeFailure::Abort, &limits, &ctx).contains("cancelled"));
        assert!(
            describe_failure(RunCodeFailure::InvalidOutput, &limits, &ctx)
                .contains("could not be serialized")
        );
        assert!(describe_failure(RunCodeFailure::OutputLimit, &limits, &ctx).contains("11 bytes"));
        assert!(describe_failure(RunCodeFailure::Protocol, &limits, &ctx).contains("garbage line"));
        assert!(describe_failure(RunCodeFailure::HeapLimit, &limits, &ctx).contains("512 MiB"));
        let worker = describe_failure(RunCodeFailure::WorkerExit, &limits, &ctx);
        assert!(worker.contains("3") && worker.contains("Exploded"));
        assert!(
            describe_failure(RunCodeFailure::SandboxUnavailable, &limits, &ctx)
                .contains("sandbox was unavailable")
        );
    }

    #[test]
    fn worker_exit_without_stderr_is_still_described() {
        let ctx = FailureContext {
            program_error: None,
            protocol_error: None,
            exit_code: None,
            stderr: "",
            output_bytes: 0,
        };
        let described =
            describe_failure(RunCodeFailure::WorkerExit, &RunCodeLimits::default(), &ctx);
        assert!(described.contains("signal"), "{described}");
    }

    // ── Goal 416 — local-sandbox security baseline ─────────────────────────

    /// Whether a real `node` runtime is reachable — same direct probe as the
    /// tool-layer tests, so a resolution bug fails loudly instead of skipping.
    fn node_available() -> bool {
        std::process::Command::new("node")
            .arg("--version")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    /// A POSIX-sh runtime shim: records every argv it is invoked with and
    /// speaks just enough of the protocol to finish a run. When called with
    /// only the probe invocation (flag + `-e` + `process.exit(0)`) it exits
    /// 0 — the "supported runtime" side of the fixture. The argv file is
    /// baked into the script (the run's env is cleared by design, so nothing
    /// else could hand it over).
    fn write_runtime_shim(
        dir: &Path,
        verdict: ShimVerdict,
        argv_file: &Path,
    ) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let path = dir.join(format!(
            "shim-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(&path, shim_source(verdict, argv_file))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        }
        Ok(path)
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum ShimVerdict {
        /// Accepts `--permission` (probe exits 0), then runs the protocol.
        Supported,
        /// Rejects every permission flag — an old runtime without a model.
        Unsupported,
    }

    fn shim_source(verdict: ShimVerdict, argv_file: &Path) -> String {
        let probe_exit = match verdict {
            ShimVerdict::Supported => 0,
            ShimVerdict::Unsupported => 9,
        };
        let argv_file_display = argv_file.display();
        format!(
            r#"#!/bin/sh
# Test fixture: a "runtime" whose only behavior is argv capture + a fixed
# protocol reply. A probe invocation is exactly three args (permission flag,
# `-e`, `process.exit(0)`) and exits {probe_exit} to encode the verdict; a
# real spawn (bootstrap source as the `-e` payload) reads the program line
# from stdin and answers one result line.
is_probe=0
if [ "$#" -eq 3 ]; then
  last=""
  for arg in "$@"; do last="$arg"; done
  [ "$last" = "process.exit(0)" ] && is_probe=1
fi
ARGV_FILE="{argv_file_display}"
for arg in "$@"; do
  printf '%s\n' "$arg" >> "$ARGV_FILE"
done
printf -- '---\n' >> "$ARGV_FILE"
[ "$is_probe" = 1 ] && exit {probe_exit}
read -r line
printf '%s\n' '{{"t":"result","value":"shim-ok"}}'
exit 0
"#
        )
    }

    fn shim_request(node: &Path, read_root: PathBuf) -> RunProgramRequest {
        RunProgramRequest {
            source: "return 'ignored';".to_string(),
            bindings: BindingTable::empty(),
            invoker: Arc::new(NullInvoker),
            limits: RunCodeLimits::default(),
            node_bin: Some(node.to_path_buf()),
            abort: None,
            read_root: Some(read_root),
            min_free_bytes: None,
        }
    }

    /// A stand-in invoker for shim runs that never issue tool calls.
    struct NullInvoker;

    #[async_trait]
    impl ToolInvoker for NullInvoker {
        async fn invoke(&self, _tool: &str, _args: Value) -> crate::error::Result<String> {
            Ok(String::new())
        }
    }

    /// Headline (#134 W1): a supported runtime is launched WITH the
    /// permission flags — fs writes confined to the scratch dir, process
    /// spawning and workers denied, reads limited to the read root. The shim
    /// records its spawn argv; the assertion reads them back.
    #[tokio::test]
    async fn runtime_launches_with_permission_flags() {
        let tmp = std::env::temp_dir().join(format!("ptc-shim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let argv_file = tmp.join("argv.txt");
        let node = write_runtime_shim(&tmp, ShimVerdict::Supported, &argv_file).unwrap();

        let read_root = tmp.clone();
        let report = run_program(shim_request(&node, read_root)).await;

        assert_eq!(report.failure, None, "{:?}", report.detail);
        assert_eq!(report.value.as_deref(), Some("shim-ok"));
        let argv = std::fs::read_to_string(&argv_file).unwrap();
        // Each shim invocation appends its argv then a `---` separator; the
        // spawn is the LAST complete block.
        let spawn_argv = argv
            .split("---\n")
            .filter(|block| !block.trim().is_empty())
            .last()
            .expect("the shim records its spawn argv");
        assert!(
            spawn_argv.contains("--permission\n"),
            "spawn argv must carry the permission flag: {spawn_argv:?}"
        );
        assert!(
            spawn_argv.contains("--allow-fs-write="),
            "spawn argv must confine writes: {spawn_argv:?}"
        );
        assert!(
            !spawn_argv.contains("--allow-child-process"),
            "process spawning must stay denied: {spawn_argv:?}"
        );
        assert!(
            !spawn_argv.contains("--allow-worker"),
            "worker threads must stay denied: {spawn_argv:?}"
        );
        assert!(
            spawn_argv.contains(&format!("--allow-fs-read={}", tmp.display())),
            "reads must be scoped to the read root: {spawn_argv:?}"
        );
        // Linux adds the /proc grant so the child can observe its own
        // address-space limit (the live regression reads
        // /proc/self/limits); macOS stays without it.
        assert_eq!(
            spawn_argv.contains("--allow-fs-read=/proc\n"),
            cfg!(target_os = "linux"),
            "/proc read grant must match the platform: {spawn_argv:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Headline (#134 W1): a runtime without a permission model is rejected
    /// loudly — `sandbox-unavailable`, with the probe's concrete reason in
    /// the detail. Never a silent fall-back to an unsandboxed run.
    #[tokio::test]
    async fn unsupported_runtime_is_rejected_loudly() {
        let tmp = std::env::temp_dir().join(format!("ptc-shim-unsup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let argv_file = tmp.join("argv.txt");
        let node = write_runtime_shim(&tmp, ShimVerdict::Unsupported, &argv_file).unwrap();

        let read_root = tmp.clone();
        let report = run_program(shim_request(&node, read_root)).await;

        assert_eq!(
            report.failure,
            Some(protocol::RunCodeFailure::SandboxUnavailable)
        );
        let detail = report
            .detail
            .clone()
            .expect("the rejection names its reason");
        assert!(
            detail.contains("no usable permission model"),
            "the reason must say the permission model is missing: {detail}"
        );
        assert!(
            detail.contains("--permission"),
            "the reason names the probed flag: {detail}"
        );
        let rendered = report.render();
        assert!(
            rendered.contains("status=sandbox-unavailable"),
            "{rendered}"
        );
        // No run happened, so no scratch dir was minted for it.
        assert!(report.sandbox.scratch_dir.is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Headline (#134 W1): the scratch dir is created for the run, is the
    /// spawn cwd, and is gone after the run — success, timeout and abort all
    /// funnel through the same cleanup.
    #[tokio::test]
    async fn scratch_dir_is_created_and_cleaned_up() {
        if !node_available() {
            eprintln!("skipping: no `node` runtime found on PATH");
            return;
        }
        let report = run_program(RunProgramRequest {
            source: r#"require('fs').writeFileSync('left-in-scratch.txt', 'x');
return process.cwd();"#
                .to_string(),
            bindings: BindingTable::empty(),
            invoker: Arc::new(NullInvoker),
            limits: RunCodeLimits::default(),
            node_bin: None,
            abort: None,
            read_root: None,
            min_free_bytes: None,
        })
        .await;
        assert_eq!(report.failure, None, "{:?}", report.detail);
        let scratch = report
            .sandbox
            .scratch_dir
            .clone()
            .expect("a completed run reports its scratch dir");
        // macOS hands back `/private/var/...` for the process cwd while the
        // fact string keeps the caller's `/var/...` spelling — same dir. The
        // scratch dir is already deleted (cleanup ran), so canonicalise via
        // its existing parent and re-attach the final component.
        let resolve = |p: &Path| -> PathBuf {
            if let Ok(resolved) = std::fs::canonicalize(p) {
                return resolved;
            }
            match (p.parent(), p.file_name()) {
                (Some(parent), Some(name)) => std::fs::canonicalize(parent)
                    .unwrap_or_else(|_| parent.to_path_buf())
                    .join(name),
                _ => p.to_path_buf(),
            }
        };
        let reported_cwd = report.value.as_deref().map(Path::new).map(resolve);
        let expected = resolve(Path::new(&scratch));
        assert!(
            reported_cwd.as_deref() == Some(expected.as_path()),
            "the spawn cwd is the scratch dir: {reported_cwd:?} vs {expected:?}",
        );
        assert!(
            !Path::new(&scratch).exists(),
            "the scratch dir is cleaned up after the run: {scratch}"
        );
    }

    /// The scratch cleanup must also run on the timeout path — a killed run
    /// may not strand its scratch dir.
    #[tokio::test]
    async fn scratch_dir_is_cleaned_up_after_a_timeout() {
        if !node_available() {
            eprintln!("skipping: no `node` runtime found on PATH");
            return;
        }
        let report = run_program(RunProgramRequest {
            source: "await new Promise(() => {});".to_string(),
            bindings: BindingTable::empty(),
            invoker: Arc::new(NullInvoker),
            limits: RunCodeLimits {
                timeout: Duration::from_millis(300),
                ..RunCodeLimits::default()
            },
            node_bin: None,
            abort: None,
            read_root: None,
            min_free_bytes: None,
        })
        .await;
        assert_eq!(report.failure, Some(protocol::RunCodeFailure::Timeout));
        let scratch = report
            .sandbox
            .scratch_dir
            .clone()
            .expect("the run minted a scratch dir before spawn");
        assert!(
            !Path::new(&scratch).exists(),
            "the killed run's scratch dir is cleaned up: {scratch}"
        );
    }

    /// Headline (#134 W1): a scratch volume below the free-space floor is
    /// rejected BEFORE spawn — `sandbox-unavailable` with the volume path and
    /// the measured free space, and no runtime process is ever started.
    #[tokio::test]
    async fn low_disk_rejects_before_spawn() {
        let node = match super::locate_node() {
            Some(node) => node,
            None => {
                eprintln!("skipping: no `node` runtime found on PATH");
                return;
            }
        };
        let report = run_program(RunProgramRequest {
            source: "return 1;".to_string(),
            bindings: BindingTable::empty(),
            invoker: Arc::new(NullInvoker),
            limits: RunCodeLimits::default(),
            node_bin: Some(node),
            abort: None,
            read_root: None,
            // An impossible floor: everything short of ~16 EiB is "low disk".
            min_free_bytes: Some(u64::MAX / 2),
        })
        .await;
        assert_eq!(
            report.failure,
            Some(protocol::RunCodeFailure::SandboxUnavailable)
        );
        let detail = report
            .detail
            .clone()
            .expect("the rejection explains itself");
        assert!(
            detail.contains("insufficient free space"),
            "the reason names the disk: {detail}"
        );
        assert!(
            detail.contains("bytes free"),
            "the reason reports the measured free space: {detail}"
        );
        assert!(
            report.calls == 0 && report.output.is_empty(),
            "no runtime work happened: {report:?}"
        );
        // The rejected run's scratch dir was cleaned up on the way out.
        if let Some(scratch) = &report.sandbox.scratch_dir {
            assert!(
                !Path::new(scratch).exists(),
                "the rejected run leaves no scratch dir behind: {scratch}"
            );
        }
    }

    /// The scratch dir carries the run's identity and lives under the system
    /// temp volume.
    #[test]
    fn create_scratch_dir_is_unique_and_inside_temp() {
        let a = create_scratch_dir().unwrap();
        let b = create_scratch_dir().unwrap();
        assert_ne!(a, b, "two live runs never share a scratch dir");
        assert!(a.starts_with(std::env::temp_dir()));
        assert!(a.is_dir());
        cleanup_scratch(&a);
        cleanup_scratch(&b);
        assert!(!a.exists() && !b.exists());
    }

    /// The address-space limit tracks the heap budget plus headroom, floored
    /// so a small test budget cannot strand the runtime at startup.
    #[test]
    fn address_space_limit_tracks_the_heap_budget() {
        let base = address_space_limit_bytes(512);
        assert_eq!(base, (512 + RLIMIT_HEADROOM_MIB) as u64 * 1024 * 1024);
        // A tiny heap budget still gets the floor.
        let tiny = address_space_limit_bytes(8);
        assert!(tiny >= 256 * 1024 * 1024, "the floor holds: {tiny}");
        // The headroom exists: the limit is strictly above the in-heap cap.
        assert!(base > 512 * 1024 * 1024);
    }

    /// The facts never claim an unenforced limit: Linux states enforcement,
    /// kernels that refuse RLIMIT_AS state exactly that.
    #[test]
    fn resource_guard_fact_matches_the_enforcement_completeness() {
        let fact = address_space_guard_fact(512);
        if cfg!(target_os = "linux") {
            assert!(fact.starts_with("rlimit_as=enforced("), "{fact}");
            assert!(fact.contains("headroom"), "{fact}");
        } else {
            assert!(
                fact.starts_with("rlimit_as=not-enforced-by-kernel"),
                "no unenforced cap may be claimed: {fact}"
            );
            assert!(fact.contains("heap limit"), "{fact}");
        }
    }

    #[tokio::test]
    async fn probe_cache_is_per_binary_path() {
        let a = PathBuf::from("/definitely/not/a/runtime-a");
        let b = PathBuf::from("/definitely/not/a/runtime-b");
        {
            let mut cache = probe_cache().lock().unwrap();
            cache.insert(
                a.clone(),
                ProbeVerdict::Supported(PermissionProbe {
                    flag: "--permission",
                }),
            );
            cache.insert(b.clone(), ProbeVerdict::Unsupported("stubbed".to_string()));
        }
        assert!(matches!(
            probe_permission_model(&a).await,
            ProbeVerdict::Supported(PermissionProbe {
                flag: "--permission"
            })
        ));
        assert!(matches!(
            probe_permission_model(&b).await,
            ProbeVerdict::Unsupported(reason) if reason == "stubbed"
        ));
        probe_cache().lock().unwrap().remove(&a);
        probe_cache().lock().unwrap().remove(&b);
    }
}
