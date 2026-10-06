//! Wire protocol + failure taxonomy for `run_code` (issue #134).
//!
//! The program runs in a fresh Node process. Its stdout carries one JSON
//! object per line (`NodeEvent`); the host's replies travel back over stdin.
//! Keeping the taxonomy in one place is what lets the tool report *why* a run
//! ended without conflating it with whether the sandbox was available at all
//! (DSH `ptc-runtime` README:129–130).

use serde::Deserialize;
use serde_json::Value;

/// A message the bootstrap emits on stdout.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "t", rename_all = "kebab-case")]
pub enum NodeEvent {
    /// A `console.*` call (or a raw stdout write funnelled through the same
    /// shim).
    Log { level: String, text: String },
    /// The program called a tool binding; the host must execute it and write a
    /// `call-result` line back to stdin.
    Call {
        id: u64,
        tool: String,
        #[serde(default)]
        args: Value,
    },
    /// The program finished normally; `value` is its serialized completion
    /// value (`None` for `undefined`).
    Result { value: Option<String> },
    /// The program threw (or its completion value could not be serialized).
    Error {
        name: String,
        #[serde(default)]
        message: String,
        #[serde(default)]
        stack: String,
    },
}

/// Parse one stdout line into a [`NodeEvent`]. A parse failure is the
/// `protocol` failure class — the program (or a stray write) corrupted the
/// stream.
pub fn parse_event(line: &str) -> Result<NodeEvent, serde_json::Error> {
    serde_json::from_str(line)
}

/// The orthogonal failure taxonomy. Every run ends in exactly one of these or
/// in success.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunCodeFailure {
    /// The program threw.
    Exception,
    /// The run exceeded its wall-clock budget and was killed.
    Timeout,
    /// The host cancelled the run.
    Abort,
    /// The runtime process died without a program-level error (crash, `exit()`,
    /// signal).
    WorkerExit,
    /// The completion value could not be serialized.
    InvalidOutput,
    /// The output budget was exhausted; the retained prefix is reported.
    OutputLimit,
    /// The stdout stream did not parse as protocol messages.
    Protocol,
    /// No usable runtime was found (or it could not be started) — a *sandbox*
    /// fact, reported separately from the program's own success or failure.
    SandboxUnavailable,
    /// The runtime exceeded its heap budget (V8 abort).
    HeapLimit,
}

impl RunCodeFailure {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exception => "exception",
            Self::Timeout => "timeout",
            Self::Abort => "abort",
            Self::WorkerExit => "worker-exit",
            Self::InvalidOutput => "invalid-output",
            Self::OutputLimit => "output-limit",
            Self::Protocol => "protocol",
            Self::SandboxUnavailable => "sandbox-unavailable",
            Self::HeapLimit => "heap-limit",
        }
    }
}

/// A program-level error reported by the bootstrap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramError {
    pub name: String,
    pub message: String,
}

/// Everything the classifier needs to decide *why* a run ended. Kept as plain
/// data so the classification is unit-testable without spawning anything.
#[derive(Debug, Clone, Default)]
pub struct ExitOutcome {
    pub timed_out: bool,
    pub aborted: bool,
    pub exit_code: Option<i32>,
    /// The process was terminated by a signal rather than exiting.
    pub killed_by_signal: bool,
    pub stderr: String,
    pub program_error: Option<ProgramError>,
    pub protocol_error: Option<String>,
    pub output_truncated: bool,
}

/// V8's out-of-memory abort signature (the message varies by Node version).
pub fn is_heap_oom(stderr: &str) -> bool {
    stderr.contains("JavaScript heap out of memory")
        || stderr.contains("Reached heap limit")
        || stderr.contains("Allocation failed")
}

impl ExitOutcome {
    /// Classify the run. `None` means success.
    pub fn classify(&self) -> Option<RunCodeFailure> {
        if self.aborted {
            return Some(RunCodeFailure::Abort);
        }
        if self.timed_out {
            return Some(RunCodeFailure::Timeout);
        }
        if let Some(err) = &self.program_error {
            if is_heap_oom(&err.message) {
                return Some(RunCodeFailure::HeapLimit);
            }
            return Some(match err.name.as_str() {
                "InvalidOutput" => RunCodeFailure::InvalidOutput,
                _ => RunCodeFailure::Exception,
            });
        }
        if self.protocol_error.is_some() {
            return Some(RunCodeFailure::Protocol);
        }
        let abnormal = self.killed_by_signal
            || matches!(self.exit_code, Some(code) if code != 0)
            || self.exit_code.is_none();
        if abnormal {
            if is_heap_oom(&self.stderr) {
                return Some(RunCodeFailure::HeapLimit);
            }
            return Some(RunCodeFailure::WorkerExit);
        }
        if self.output_truncated {
            return Some(RunCodeFailure::OutputLimit);
        }
        None
    }
}

/// Facts about the environment the program ran in. Reported separately from
/// the run's success or failure.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SandboxFacts {
    /// Resolved runtime binary, or `None` when the sandbox was unavailable.
    pub runtime: Option<String>,
    /// Environment policy applied to the fresh process.
    pub env_policy: String,
    pub heap_limit_mib: usize,
    pub output_limit_bytes: usize,
    pub timeout_secs: u64,
}

/// The single value `run_code` renders for the model.
#[derive(Debug, Clone)]
pub struct RunReport {
    pub failure: Option<RunCodeFailure>,
    pub exit_code: Option<i32>,
    pub calls: usize,
    pub duration_ms: u128,
    pub output: String,
    pub output_bytes: usize,
    pub output_truncated: bool,
    pub value: Option<String>,
    pub detail: Option<String>,
    pub sandbox: SandboxFacts,
}

impl RunReport {
    /// A single text observation: sandbox facts, the classified status, the
    /// retained output, and (when present) the completion value.
    pub fn render(&self) -> String {
        let status = self.failure.map(|f| f.as_str()).unwrap_or("ok");
        let exit = match self.exit_code {
            Some(code) => code.to_string(),
            None => "none".to_string(),
        };
        let mut out = format!(
            "[run_code] status={status} exit={exit} calls={} duration_ms={} \
             output_bytes={} truncated={}\n",
            self.calls, self.duration_ms, self.output_bytes, self.output_truncated
        );
        out.push_str(&format!(
            "sandbox: runtime={} env={} heap_limit_mib={} output_limit_bytes={} timeout_secs={}\n",
            self.sandbox.runtime.as_deref().unwrap_or("unavailable"),
            self.sandbox.env_policy,
            self.sandbox.heap_limit_mib,
            self.sandbox.output_limit_bytes,
            self.sandbox.timeout_secs,
        ));
        if let Some(detail) = &self.detail {
            out.push_str(&format!("detail: {detail}\n"));
        }
        out.push_str("--- output ---\n");
        if self.output.is_empty() {
            out.push_str("(empty)\n");
        } else {
            out.push_str(&self.output);
            if !self.output.ends_with('\n') {
                out.push('\n');
            }
        }
        if let Some(value) = &self.value {
            out.push_str("--- value ---\n");
            out.push_str(value);
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_every_bootstrap_event() {
        assert_eq!(
            parse_event(r#"{"t":"log","level":"warn","text":"hi"}"#).unwrap(),
            NodeEvent::Log {
                level: "warn".into(),
                text: "hi".into()
            }
        );
        assert_eq!(
            parse_event(r#"{"t":"call","id":7,"tool":"Read","args":{"path":"a"}}"#).unwrap(),
            NodeEvent::Call {
                id: 7,
                tool: "Read".into(),
                args: json!({"path": "a"})
            }
        );
        assert_eq!(
            parse_event(r#"{"t":"call","id":7,"tool":"Read"}"#).unwrap(),
            NodeEvent::Call {
                id: 7,
                tool: "Read".into(),
                args: Value::Null
            }
        );
        assert_eq!(
            parse_event(r#"{"t":"result","value":null}"#).unwrap(),
            NodeEvent::Result { value: None }
        );
        assert_eq!(
            parse_event(r#"{"t":"error","name":"Error","message":"boom"}"#).unwrap(),
            NodeEvent::Error {
                name: "Error".into(),
                message: "boom".into(),
                stack: String::new()
            }
        );
    }

    #[test]
    fn unparsable_lines_are_protocol_errors() {
        assert!(parse_event("not json").is_err());
        assert!(parse_event(r#"{"t":"mystery"}"#).is_err());
    }

    #[test]
    fn classification_prefers_the_host_side_causes() {
        let outcome = ExitOutcome {
            timed_out: true,
            ..ExitOutcome::default()
        };
        assert_eq!(outcome.classify(), Some(RunCodeFailure::Timeout));

        let outcome = ExitOutcome {
            aborted: true,
            timed_out: true,
            ..ExitOutcome::default()
        };
        assert_eq!(outcome.classify(), Some(RunCodeFailure::Abort));
    }

    #[test]
    fn classification_distinguishes_program_failures() {
        let exception = ExitOutcome {
            exit_code: Some(1),
            program_error: Some(ProgramError {
                name: "TypeError".into(),
                message: "x is not a function".into(),
            }),
            ..ExitOutcome::default()
        };
        assert_eq!(exception.classify(), Some(RunCodeFailure::Exception));

        let invalid = ExitOutcome {
            exit_code: Some(1),
            program_error: Some(ProgramError {
                name: "InvalidOutput".into(),
                message: "circular".into(),
            }),
            ..ExitOutcome::default()
        };
        assert_eq!(invalid.classify(), Some(RunCodeFailure::InvalidOutput));

        let protocol = ExitOutcome {
            protocol_error: Some("garbage".into()),
            ..ExitOutcome::default()
        };
        assert_eq!(protocol.classify(), Some(RunCodeFailure::Protocol));
    }

    #[test]
    fn classification_detects_the_heap_limit_from_stderr() {
        let outcome = ExitOutcome {
            exit_code: Some(134),
            killed_by_signal: true,
            stderr:
                "FATAL ERROR: Reached heap limit Allocation failed - JavaScript heap out of memory"
                    .into(),
            ..ExitOutcome::default()
        };
        assert_eq!(outcome.classify(), Some(RunCodeFailure::HeapLimit));

        let plain_crash = ExitOutcome {
            exit_code: Some(1),
            ..ExitOutcome::default()
        };
        assert_eq!(plain_crash.classify(), Some(RunCodeFailure::WorkerExit));
    }

    #[test]
    fn classification_reports_output_limit_only_for_a_completed_run() {
        let outcome = ExitOutcome {
            exit_code: Some(0),
            output_truncated: true,
            ..ExitOutcome::default()
        };
        assert_eq!(outcome.classify(), Some(RunCodeFailure::OutputLimit));

        let clean = ExitOutcome {
            exit_code: Some(0),
            output_truncated: false,
            ..ExitOutcome::default()
        };
        assert_eq!(clean.classify(), None);
    }

    #[test]
    fn failure_status_strings_are_stable() {
        let cases = [
            (RunCodeFailure::Exception, "exception"),
            (RunCodeFailure::Timeout, "timeout"),
            (RunCodeFailure::Abort, "abort"),
            (RunCodeFailure::WorkerExit, "worker-exit"),
            (RunCodeFailure::InvalidOutput, "invalid-output"),
            (RunCodeFailure::OutputLimit, "output-limit"),
            (RunCodeFailure::Protocol, "protocol"),
            (RunCodeFailure::SandboxUnavailable, "sandbox-unavailable"),
            (RunCodeFailure::HeapLimit, "heap-limit"),
        ];
        for (failure, expected) in cases {
            assert_eq!(failure.as_str(), expected);
            assert_eq!(
                serde_json::to_string(&failure).unwrap(),
                format!("\"{expected}\"")
            );
        }
    }

    #[test]
    fn render_reports_sandbox_facts_separately_from_failure() {
        let report = RunReport {
            failure: Some(RunCodeFailure::SandboxUnavailable),
            exit_code: None,
            calls: 0,
            duration_ms: 3,
            output: String::new(),
            output_bytes: 0,
            output_truncated: false,
            value: None,
            detail: Some("node not found on PATH".into()),
            sandbox: SandboxFacts {
                runtime: None,
                env_policy: "cleared".into(),
                heap_limit_mib: 512,
                output_limit_bytes: 64,
                timeout_secs: 120,
            },
        };
        let rendered = report.render();
        assert!(rendered.contains("status=sandbox-unavailable"));
        assert!(rendered.contains("sandbox: runtime=unavailable"));
        assert!(rendered.contains("--- output ---\n(empty)"));
        assert!(rendered.contains("detail: node not found on PATH"));
    }

    #[test]
    fn render_includes_the_completion_value() {
        let report = RunReport {
            failure: None,
            exit_code: Some(0),
            calls: 2,
            duration_ms: 12,
            output: "done\n".into(),
            output_bytes: 5,
            output_truncated: false,
            value: Some("{\"total\":3}".into()),
            detail: None,
            sandbox: SandboxFacts {
                runtime: Some("/usr/bin/node".into()),
                env_policy: "cleared".into(),
                heap_limit_mib: 512,
                output_limit_bytes: 64,
                timeout_secs: 120,
            },
        };
        let rendered = report.render();
        assert!(rendered.contains("status=ok"));
        assert!(rendered.contains("--- value ---\n{\"total\":3}"));
    }
}
