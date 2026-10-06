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
use std::path::PathBuf;
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
    } = req;
    let limits = limits.clamped();
    let started = Instant::now();
    let mut sandbox = SandboxFacts {
        runtime: None,
        env_policy: ENV_POLICY.to_string(),
        heap_limit_mib: limits.heap_limit_mib,
        output_limit_bytes: limits.output_limit_bytes,
        timeout_secs: limits.timeout.as_secs(),
    };

    let Some(node) = node_bin.or_else(locate_node) else {
        return unavailable_report(
            sandbox,
            started,
            &format!("no JavaScript runtime found (install `node` or set {NODE_ENV_VAR})"),
        );
    };
    sandbox.runtime = Some(node.display().to_string());

    let mut cmd = Command::new(&node);
    cmd.arg(format!("--max-old-space-size={}", limits.heap_limit_mib));
    cmd.arg("-e");
    cmd.arg(BOOTSTRAP_SOURCE);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    cmd.env_clear();
    for (key, value) in child_env() {
        cmd.env(key, value);
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
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
                                    tokio::spawn(async move {
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
                                        if let Some(handle) = guard.as_mut() {
                                            let _ = handle.write_all(line.as_bytes()).await;
                                            let _ = handle.flush().await;
                                        }
                                    });
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
        let _ = child.start_kill();
    }
    let status = match tokio::time::timeout(REAP_GRACE, child.wait()).await {
        Ok(Ok(status)) => Some(status),
        Ok(Err(_)) | Err(_) => None,
    };

    let stderr = match stderr_task {
        Some(task) => task.await.unwrap_or_default(),
        None => String::new(),
    };

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
}
