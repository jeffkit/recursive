//! `recursive resume` surfaces that `main()`/`resume.rs` gate on predicates.
//!
//! Pins user-visible behaviours that only appear when the real binary runs:
//!
//! * the orphan-tool-call block runs at all (`if !orphans.is_empty()`),
//! * `--orphans=skip` answers every orphan with a synthetic tool result and
//!   persists it into the session,
//! * only *External*-classified orphans get the re-execution warning,
//! * a drifted `tool_registry_hash` hard-fails without `--allow-tool-drift`
//!   and degrades to a warning (+ vanished-tool report) with it; redo of a
//!   vanished tool still refuses,
//! * a JSON-output, non-headless resume serves host `control_request` frames
//!   (the `json_mode && !config.headless` control bridge),
//! * `--session-out` is written only for a *non*-clean finish.
//!
//! Unix-only：Rig 直接 spawn 真实二进制 + stub turn，windows 上 stub resume
//! 非零退出（--session-out legacy 警告路径，2026-09-30 CI 实证），属 windows
//! 行为差异而非回归——需要 windows 覆盖时按 .dev/goals/410-windows-session-
//! recording-path.md 修 resume 的 windows 路径再解封。
#![cfg(unix)]

use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const OPENAI_TEXT: &str = concat!(
    r#"{"choices":[{"message":{"role":"assistant","content":"resumed and done"},"#,
    r#""finish_reason":"stop"}],"#,
    r#""usage":{"prompt_tokens":3,"completion_tokens":5,"total_tokens":8}}"#
);

/// Minimal fake OpenAI endpoint: answers every request with a final text turn,
/// or — in `fail` mode — a 400 so the provider surfaces an error to the caller.
struct Stub {
    addr: SocketAddr,
}

impl Stub {
    fn start() -> Self {
        Self::start_with(false)
    }

    /// A stub whose every request is answered with an OpenAI-shaped 400.
    fn start_failing() -> Self {
        Self::start_with(true)
    }

    fn start_with(fail: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
        let addr = listener.local_addr().expect("stub addr");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                std::thread::spawn(move || serve(stream, fail));
            }
        });
        Self { addr }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

fn serve(mut stream: TcpStream, fail: bool) {
    // An accepted socket inherits non-blocking mode on macOS: put it back to
    // blocking before reading the request, or the read races the client write.
    let _ = stream.set_nonblocking(false);
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut head_len = None;
    let mut content_length = 0usize;
    loop {
        if head_len.is_none() {
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                head_len = Some(pos + 4);
                let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                for line in head.lines() {
                    let lower = line.to_ascii_lowercase();
                    if let Some(value) = lower.strip_prefix("content-length:") {
                        content_length = value.trim().parse().unwrap_or(0);
                    }
                }
            }
        }
        if let Some(head) = head_len {
            if buf.len() >= head + content_length {
                break;
            }
        }
        match std::io::Read::read(&mut stream, &mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    if fail {
        let body = r#"{"error":{"message":"stub provider failure","type":"server_error"}}"#;
        let _ = write!(
            stream,
            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.flush();
        return;
    }
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        OPENAI_TEXT.len(),
        OPENAI_TEXT
    );
    let _ = stream.flush();
}

struct Rig {
    home: tempfile::TempDir,
    workspace: tempfile::TempDir,
    stub: Stub,
}

impl Rig {
    fn new() -> Self {
        Self::with_stub(Stub::start())
    }

    fn with_stub(stub: Stub) -> Self {
        Self {
            home: tempfile::tempdir().expect("home tempdir"),
            workspace: tempfile::tempdir().expect("workspace tempdir"),
            stub,
        }
    }

    /// A JSONL session directory. `tool_calls` is the assistant entry's tool
    /// call list; an unanswered call is an orphan. No `tool_registry_hash` is
    /// recorded, so the resume only warns about the pre-g151 record.
    /// `tool_hash`, when given, is written as the session's recorded hash —
    /// pass a bogus string to simulate post-upgrade registry drift.
    fn session_dir(&self, id: &str, tool_names: &[&str]) -> PathBuf {
        self.session_dir_with_hash(id, tool_names, None)
    }

    fn session_dir_with_hash(
        &self,
        id: &str,
        tool_names: &[&str],
        tool_hash: Option<&str>,
    ) -> PathBuf {
        let dir = self.home.path().join("sessions").join(id);
        std::fs::create_dir_all(&dir).expect("mkdir session dir");
        let mut meta = serde_json::json!({
            "session_id": id,
            "goal": "g",
            "model": "test-model",
            "provider": "openai",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "message_count": 2,
        });
        if let Some(h) = tool_hash {
            meta["tool_registry_hash"] = serde_json::json!(h);
        }
        std::fs::write(
            dir.join(".meta.json"),
            serde_json::to_vec(&meta).expect("meta"),
        )
        .expect("write meta");

        let mut lines = String::new();
        lines.push_str(
            "{\"id\":\"m1\",\"role\":\"user\",\"content\":\"go\",\"timestamp\":\"2026-01-01T00:00:00Z\"}\n",
        );
        let calls: Vec<serde_json::Value> = tool_names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                serde_json::json!({"id": format!("c{i}"), "name": name, "arguments": {}})
            })
            .collect();
        let assistant = serde_json::json!({
            "id": "m2",
            "role": "assistant",
            "content": "working",
            "tool_calls": calls,
            "timestamp": "2026-01-01T00:00:00Z",
        });
        lines.push_str(&format!("{assistant}\n"));
        std::fs::write(dir.join("transcript.jsonl"), lines).expect("write transcript");
        dir
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_recursive"));
        cmd.env_clear();
        cmd.env("HOME", self.home.path());
        cmd.env("RECURSIVE_HOME", self.home.path());
        cmd.env("RECURSIVE_RETRY_MAX", "0");
        if let Some(path) = std::env::var_os("PATH") {
            cmd.env("PATH", path);
        }
        cmd.current_dir(self.workspace.path());
        cmd.arg("--log").arg("error");
        cmd.arg("--workspace").arg(self.workspace.path());
        cmd.arg("--api-base").arg(self.stub.base_url());
        cmd.arg("--api-key").arg("sk-test-key");
        cmd.arg("--model").arg("test-model");
        cmd.arg("--provider").arg("openai");
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd
    }

    fn run(&self, args: &[&str], stdin: Option<&str>) -> Output {
        let mut cmd = self.cmd();
        cmd.args(args);
        match stdin {
            None => cmd.output().expect("spawn recursive"),
            Some(input) => {
                cmd.stdin(Stdio::piped());
                let mut child = cmd.spawn().expect("spawn recursive");
                {
                    let mut pipe = child.stdin.take().expect("child stdin");
                    pipe.write_all(input.as_bytes()).expect("write stdin");
                }
                child.wait_with_output().expect("wait for recursive")
            }
        }
    }
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// Global flags must come *before* the subcommand (clap ordering); `extra` is
/// therefore split into `(globals, resume-specific)`.
fn resume_args(dir: &Path, globals: &[&str], extra: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = globals.iter().map(|s| s.to_string()).collect();
    args.push("resume".into());
    args.push("--from-file".into());
    args.push(dir.display().to_string());
    args.push("-p".into());
    args.push("go".into());
    args.extend(extra.iter().map(|s| s.to_string()));
    args
}

fn as_refs(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

// ── tool-registry drift (--allow-tool-drift) ───────────────────────────────

#[test]
fn resume_still_refuses_a_drifted_registry_without_the_flag() {
    let rig = Rig::new();
    let dir = rig.session_dir_with_hash("sess-drift-hard", &[], Some("bogus-old-hash"));

    let out = rig.run(&as_refs(&resume_args(&dir, &[], &[])), None);
    let stderr = stderr_of(&out);

    assert!(!out.status.success(), "drifted hash must fail the resume");
    assert!(
        stderr.contains("tool registry hash mismatch"),
        "the mismatch must be named, got:\n{stderr}"
    );
    assert!(
        stderr.contains("--allow-tool-drift"),
        "the error must point at the escape hatch, got:\n{stderr}"
    );
}

#[test]
fn resume_allow_tool_drift_degrades_mismatch_to_a_warning() {
    let rig = Rig::new();
    let dir = rig.session_dir_with_hash("sess-drift-ok", &[], Some("bogus-old-hash"));

    let out = rig.run(
        &as_refs(&resume_args(&dir, &[], &["--allow-tool-drift"])),
        None,
    );
    let stderr = stderr_of(&out);

    assert!(
        out.status.success(),
        "--allow-tool-drift must let the resume proceed: {:?}",
        stderr
    );
    assert!(
        stderr.contains("tool registry hash mismatch"),
        "the mismatch must still be surfaced as a warning, got:\n{stderr}"
    );
    assert!(
        stderr.contains("resuming anyway"),
        "the warning must say the resume proceeds, got:\n{stderr}"
    );
}

#[test]
fn resume_allow_tool_drift_reports_vanished_referenced_tools() {
    let rig = Rig::new();
    // The transcript references a tool that cannot exist in any registry.
    // The orphan itself still follows the normal orphan policy, so pass
    // `--orphans skip` to isolate the drift behaviour under test.
    let dir = rig.session_dir_with_hash(
        "sess-drift-vanished",
        &["TotallyVanishedTool"],
        Some("bogus-old-hash"),
    );

    let out = rig.run(
        &as_refs(&resume_args(
            &dir,
            &[],
            &["--allow-tool-drift", "--orphans", "skip"],
        )),
        None,
    );
    let stderr = stderr_of(&out);

    assert!(
        out.status.success(),
        "drift + skipped orphan must resume: {:?}",
        stderr
    );
    assert!(
        stderr.contains("no longer"),
        "the drift report must name vanished tools, got:\n{stderr}"
    );
    assert!(
        stderr.contains("TotallyVanishedTool"),
        "the vanished tool must be named, got:\n{stderr}"
    );
}

#[test]
fn resume_redo_of_a_vanished_tool_refuses_even_with_allow_tool_drift() {
    let rig = Rig::new();
    let dir = rig.session_dir_with_hash(
        "sess-drift-redo",
        &["TotallyVanishedTool"],
        Some("bogus-old-hash"),
    );

    let out = rig.run(
        &as_refs(&resume_args(
            &dir,
            &[],
            &["--allow-tool-drift", "--orphans", "redo"],
        )),
        None,
    );
    let stderr = stderr_of(&out);

    assert!(
        !out.status.success(),
        "redo of a tool that no longer exists must fail even with \
         --allow-tool-drift: {:?}",
        stderr
    );
    assert!(
        stderr.contains("no longer"),
        "the refusal must name the missing tool(s), got:\n{stderr}"
    );
    assert!(
        stderr.contains("--orphans=skip"),
        "the refusal must point at a workable alternative, got:\n{stderr}"
    );
}

// ── orphan handling ────────────────────────────────────────────────────────

#[test]
fn resume_refuses_to_proceed_with_orphans_when_asked_to_abort() {
    let rig = Rig::new();
    let dir = rig.session_dir("sess-abort", &["TotallyUnknownTool"]);

    let out = rig.run(
        &as_refs(&resume_args(&dir, &[], &["--orphans", "abort"])),
        None,
    );
    let stderr = stderr_of(&out);

    assert!(!out.status.success(), "abort policy must fail the resume");
    assert!(
        stderr.contains("incomplete tool call(s)"),
        "the orphan report must be printed, got:\n{stderr}"
    );
    assert!(
        stderr.contains("refusing to resume"),
        "abort policy must refuse the resume, got:\n{stderr}"
    );
}

#[test]
fn resume_skip_policy_answers_orphans_with_a_synthetic_result() {
    let rig = Rig::new();
    let dir = rig.session_dir("sess-skip", &["TotallyUnknownTool"]);

    let out = rig.run(
        &as_refs(&resume_args(&dir, &[], &["--orphans", "skip"])),
        None,
    );
    let stderr = stderr_of(&out);

    assert!(
        stderr.contains("incomplete tool call(s)"),
        "the orphan report must be printed, got:\n{stderr}"
    );
    assert!(
        stderr.contains("synthetic interrupted result"),
        "skip policy must announce the chosen handling, got:\n{stderr}"
    );
    assert!(
        out.status.success(),
        "the repaired seed must complete the run, got:\n{stderr}"
    );

    // The answer has to land in the session itself, not only in the seed the
    // provider sees — an unpersisted repair is re-detected (and, under redo,
    // re-executed) on the next resume.
    let transcript = std::fs::read_to_string(dir.join("transcript.jsonl")).expect("transcript");
    let answer = transcript
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|e| e["role"] == "tool")
        .expect("a tool result must answer the orphan");
    assert_eq!(answer["tool_call_id"], "c0");
    assert_eq!(answer["content"], "[interrupted: no result recorded]");
}

#[test]
fn resume_warns_only_for_external_orphans_on_redo() {
    let rig = Rig::new();
    let external = rig.session_dir("sess-external", &["TotallyUnknownTool"]);
    let read_only = rig.session_dir("sess-read-only", &["Read"]);

    let out = rig.run(
        &as_refs(&resume_args(&external, &[], &["--orphans", "redo"])),
        None,
    );
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("classified External"),
        "an unknown (External-classified) orphan must warn before re-execution, got:\n{stderr}"
    );

    let out = rig.run(
        &as_refs(&resume_args(&read_only, &[], &["--orphans", "redo"])),
        None,
    );
    let stderr = stderr_of(&out);
    assert!(
        !stderr.contains("classified External"),
        "a read-only orphan must not get the external re-execution warning, got:\n{stderr}"
    );
}

// ── control bridge (json output, non-headless) ─────────────────────────────

const INTERRUPT_FRAME: &str = "{\"type\":\"control_request\",\"request_id\":\"r1\",\"request\":{\"subtype\":\"interrupt\"}}\n";

#[test]
fn resume_serves_control_frames_when_json_output_is_used() {
    let rig = Rig::new();
    let dir = rig.session_dir("sess-control", &[]);

    let out = rig.run(
        &as_refs(&resume_args(&dir, &["--output-format", "stream-json"], &[])),
        Some(INTERRUPT_FRAME),
    );
    let stdout = stdout_of(&out);

    assert!(
        stdout.contains("\"control_response\"") && stdout.contains("\"r1\""),
        "a JSON-output resume must answer host control frames, got:\n{stdout}"
    );
}

#[test]
fn resume_ignores_control_frames_without_json_output() {
    let rig = Rig::new();
    let dir = rig.session_dir("sess-no-bridge", &[]);

    let out = rig.run(
        &as_refs(&resume_args(&dir, &[], &[])),
        Some(INTERRUPT_FRAME),
    );
    let stdout = stdout_of(&out);

    assert!(
        !stdout.contains("control_response"),
        "a plain-text resume must not open the control bridge, got:\n{stdout}"
    );
}

// ── --session-out on a clean finish ────────────────────────────────────────

#[test]
fn resume_does_not_write_session_out_after_a_clean_finish() {
    let rig = Rig::new();
    let dir = rig.session_dir("sess-clean", &[]);
    let session_out = rig.workspace.path().join("finished.json");

    let out = rig.run(
        &as_refs(&resume_args(
            &dir,
            &["--session-out", session_out.to_str().expect("utf8 path")],
            &[],
        )),
        None,
    );

    assert!(
        out.status.success(),
        "the stubbed turn should finish cleanly: {:?}",
        stderr_of(&out)
    );
    assert!(
        !session_out.exists(),
        "--session-out records *interrupted* runs only; a NoMoreToolCalls finish must not write it"
    );
}

// ── issue #110: a failed resume still closes its session envelope ──────────

/// A provider failure during `resume` must finalize the session it is
/// appending to — `?` used to return before the finalize, so the session
/// stayed `active` forever. The failure text must land on `.meta.json`.
#[test]
fn resume_finalizes_the_session_as_crashed_when_the_provider_fails() {
    let rig = Rig::with_stub(Stub::start_failing());
    let dir = rig.session_dir("sess-fail", &[]);

    let out = rig.run(&as_refs(&resume_args(&dir, &[], &[])), None);
    assert!(
        !out.status.success(),
        "a failing provider must fail the resume, got:\n{}",
        stdout_of(&out)
    );

    let raw = std::fs::read(dir.join(".meta.json")).expect("read .meta.json");
    let meta: serde_json::Value =
        serde_json::from_slice(&raw).expect(".meta.json must be valid JSON");
    assert_eq!(
        meta["status"], "crashed",
        "a failed resume must close the session envelope, got: {meta}"
    );
    assert!(
        meta["error"]
            .as_str()
            .map(|e| !e.is_empty())
            .unwrap_or(false),
        "the provider failure must be carried onto the session, got: {meta}"
    );
}
