//! Human-visible stream surfaces of the `recursive` binary
//! (`cli::output::stream_events` / `stream_events_repl` / `stream_events_json`).
//!
//! Issue #245 batch 4: these printers are the only place the raw
//! `AgentEvent` lines (`[step N] …`, `[worker …]`, truncation, compaction,
//! repl `↳`) are rendered, and libtest's output capture hides every
//! `println!` a test process makes — even from spawned worker threads — so
//! the only faithful harness is the real binary against a scripted stub
//! provider. Four pins:
//!
//! * a `run` turn sequence renders assistant text (blank text stays off),
//!   tool calls/results, the 800-byte truncation boundary including a
//!   multi-byte character straddling it, latency and the turn-finished line;
//! * a run over a tiny `--max-transcript-chars` prints the compaction line;
//! * a subagent dispatch surfaces attributed `[worker …]` lines;
//! * a piped-stdin `repl` renders its bare-text/tool-hint stream, and
//!   `repl --output-format json` renders raw NDJSON events.
//!
//! The plan arms of the same two printers (`PlanProposed` / `PlanConfirmed`
//! / `PlanRejected`) are deliberately not exercised here: `run` and `repl`
//! both build their runtime with `interactive = false`, which does not
//! register `enter_plan_mode` / `exit_plan_mode` (see the builder test "a
//! headless run must not register the blocking plan tools"), so no plan
//! event can reach these printers from any surface that uses them.
//!
//! Unix-only: the harness follows `cli_signal_surfaces.rs` (raw TCP stub,
//! SIGKILL watchdog); nothing here is Windows-relevant.
#![cfg(unix)]

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What the stub provider answers for the next request in its script.
#[derive(Clone)]
enum Reply {
    /// A final text turn (`finish_reason: stop`).
    Text(String),
    /// Assistant text plus one tool call (`finish_reason: tool_calls`).
    TextAndTool {
        content: String,
        tool: &'static str,
        arguments: String,
    },
    /// A tool call with no assistant text.
    Tool {
        tool: &'static str,
        arguments: String,
    },
}

fn tool_call_body(id: &str, tool: &str, arguments: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "type": "function",
        "function": { "name": tool, "arguments": arguments }
    })
}

fn body_for(n: usize, reply: &Reply) -> String {
    let (message, finish) = match reply {
        Reply::Text(content) => (
            serde_json::json!({ "role": "assistant", "content": content }),
            "stop",
        ),
        Reply::TextAndTool {
            content,
            tool,
            arguments,
        } => (
            serde_json::json!({
                "role": "assistant",
                "content": content,
                "tool_calls": [tool_call_body(&format!("call_{n}"), tool, arguments)]
            }),
            "tool_calls",
        ),
        Reply::Tool { tool, arguments } => (
            serde_json::json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [tool_call_body(&format!("call_{n}"), tool, arguments)]
            }),
            "tool_calls",
        ),
    };
    serde_json::json!({
        "choices": [{ "message": message, "finish_reason": finish }],
        // Zero usage keeps `should_compact` on the *chars* fallback branch —
        // a non-zero last_prompt_tokens would make the preset's token
        // threshold decide, and the char budget would never fire.
        "usage": { "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 }
    })
    .to_string()
}

/// Minimal fake OpenAI endpoint: the *n*-th `/chat/completions` request gets
/// `script[n]`; a script under-run answers a final text turn so a mis-count
/// fails the line asserts instead of hanging the run. Other paths (model
/// catalog probes etc.) get a generic empty list and never touch the script.
struct Stub {
    addr: SocketAddr,
}

impl Stub {
    fn start(script: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
        let addr = listener.local_addr().expect("stub addr");
        let queue = Arc::new(Mutex::new(VecDeque::from(script)));
        let served = Arc::new(AtomicUsize::new(0));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let queue = Arc::clone(&queue);
                let served = Arc::clone(&served);
                std::thread::spawn(move || {
                    let path = read_request(&mut stream);
                    if path.ends_with("/chat/completions") {
                        let reply = {
                            let mut q = queue.lock().unwrap_or_else(|p| p.into_inner());
                            q.pop_front()
                                .unwrap_or_else(|| Reply::Text("script exhausted".into()))
                        };
                        let n = served.fetch_add(1, Ordering::SeqCst);
                        serve(stream, &body_for(n, &reply));
                    } else {
                        serve(stream, "{\"object\":\"list\",\"data\":[]}");
                    }
                });
            }
        });
        Self { addr }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

fn read_request(stream: &mut TcpStream) -> String {
    // An accepted socket inherits non-blocking mode on macOS: put it back to
    // blocking before reading the request, or the read races the client write.
    let _ = stream.set_nonblocking(false);
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut head_len = None;
    let mut content_length = 0usize;
    let mut path = String::new();
    loop {
        if head_len.is_none() {
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                head_len = Some(pos + 4);
                let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                if let Some(request_line) = head.lines().next() {
                    path = request_line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_string();
                }
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
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    path
}

fn serve(mut stream: TcpStream, body: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.flush();
}

fn read_args(rel_path: &str) -> String {
    serde_json::json!({ "path": rel_path }).to_string()
}

fn agent_args(worker: &str) -> String {
    serde_json::json!({
        "mode": "single",
        "manifest": { worker: { "system_prompt": "be brief" } },
        "prompt": "say hi"
    })
    .to_string()
}

/// `wait_with_output` with a hard ceiling: a run that never exits (a mutated
/// truncation loop can spin forever) is SIGKILLed so the test fails instead
/// of hanging the suite.
fn wait_with_timeout(child: Child, limit: Duration) -> Output {
    let pid = child.id() as libc::pid_t;
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watchdog_done = Arc::clone(&done);
    std::thread::spawn(move || {
        let started = Instant::now();
        while started.elapsed() < limit {
            if watchdog_done.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if !watchdog_done.load(Ordering::SeqCst) {
            // SAFETY: the pid is still unreaped here, so it cannot have been
            // recycled.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    });
    let out = child.wait_with_output().expect("wait for recursive");
    done.store(true, Ordering::SeqCst);
    out
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// Shared builder: stub provider + isolated RECURSIVE_HOME/workspace, tools
/// auto-approved, retries off. `extra` carries per-test flags; `env` carries
/// per-test environment.
fn run_child(
    home: &Path,
    workspace: &Path,
    stub: &Stub,
    extra: &[&str],
    env: &[(&str, &str)],
) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_recursive"));
    cmd.env_clear();
    cmd.env("HOME", home);
    cmd.env("RECURSIVE_HOME", home);
    cmd.env("RECURSIVE_RETRY_MAX", "0");
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd.current_dir(workspace);
    cmd.arg("--log").arg("error");
    cmd.arg("--workspace").arg(workspace);
    cmd.arg("--api-base").arg(stub.base_url());
    cmd.arg("--api-key").arg("sk-test-key");
    cmd.arg("--model").arg("test-model");
    cmd.arg("--provider").arg("openai");
    cmd.arg("--permission-mode").arg("auto");
    for flag in extra {
        cmd.arg(flag);
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd
}

/// Write the fixture files a run's `Read` calls will hit.
fn seed_files(workspace: &Path, files: &[(&str, Vec<u8>)]) {
    for (name, bytes) in files {
        std::fs::write(workspace.join(name), bytes).expect("seed fixture file");
    }
}

#[test]
fn run_human_stream_pins_assistant_toolresult_and_lifecycle_lines() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    seed_files(
        workspace.path(),
        &[
            ("big801.txt", "a".repeat(801).into_bytes()),
            // `日` straddles the 800th byte (799..802): the truncation walk
            // must back off to 799 instead of slicing mid-character.
            ("mb.txt", ("x".repeat(799) + "日").into_bytes()),
            ("ascii800.txt", "a".repeat(800).into_bytes()),
            ("small.txt", b"tiny".to_vec()),
        ],
    );
    let stub = Stub::start(vec![
        Reply::TextAndTool {
            content: "hello there".into(),
            tool: "Read",
            arguments: read_args("big801.txt"),
        },
        Reply::TextAndTool {
            content: "   ".into(),
            tool: "Read",
            arguments: read_args("mb.txt"),
        },
        Reply::TextAndTool {
            content: "ok".into(),
            tool: "Read",
            arguments: read_args("ascii800.txt"),
        },
        Reply::Text("all done".into()),
    ]);
    let mut cmd = run_child(home.path(), workspace.path(), &stub, &[], &[]);
    cmd.arg("run").arg("exercise the human stream");
    cmd.stdin(Stdio::null());
    let out = wait_with_timeout(
        cmd.spawn().expect("spawn recursive"),
        Duration::from_secs(120),
    );
    assert!(out.status.success(), "run must exit 0: {}", stderr_of(&out));
    let out = stdout_of(&out);

    // Assistant text renders with its step prefix; the whitespace-only step 2
    // text must stay off the stream (guard `!text.trim().is_empty()`).
    assert!(
        out.lines().any(|l| l.contains("assistant: hello there")),
        "assistant text missing: {out:?}"
    );
    assert_eq!(
        out.lines().filter(|l| l.contains("assistant:")).count(),
        3,
        "exactly the three non-blank assistant lines must print: {out:?}"
    );
    assert!(out.lines().any(|l| l.contains("assistant: ok")), "{out:?}");
    assert!(
        out.lines().any(|l| l.contains("assistant: all done")),
        "{out:?}"
    );

    // Tool calls render one line per call.
    assert_eq!(
        out.matches("] -> Read ").count(),
        3,
        "three tool-call lines expected: {out:?}"
    );

    // Tool results truncate at 800 bytes on a char boundary: the 801-byte
    // file loses its last byte, the multi-byte file backs off to 799, and
    // the exactly-800-byte file is printed verbatim with no marker.
    assert_eq!(
        out.matches("...[truncated]").count(),
        2,
        "exactly the over-boundary results are marked truncated: {out:?}"
    );
    assert!(
        out.contains(&format!("\n{}\n...[truncated]", "a".repeat(800))),
        "801-byte read must truncate to 800: {out:?}"
    );
    assert!(
        out.contains(&format!("\n{}\n...[truncated]", "x".repeat(799))),
        "multi-byte read must back off to the 799th byte: {out:?}"
    );
    assert!(
        out.contains(&format!("\n{}\n", "a".repeat(800))),
        "800-byte read is the exact boundary and must print verbatim: {out:?}"
    );

    // Latency renders once per LLM call; the turn-finished line closes the run.
    assert_eq!(
        out.matches("llm latency: ").count(),
        4,
        "one latency line per step: {out:?}"
    );
    assert!(
        out.lines()
            .any(|l| l.contains("ms") && l.contains("llm latency:")),
        "{out:?}"
    );
    assert!(
        out.lines()
            .any(|l| l.contains("steps] reason: no_more_tool_calls")),
        "turn-finished line missing: {out:?}"
    );
}

#[test]
fn resume_stream_prints_the_compaction_line_under_a_tiny_transcript_cap() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    seed_files(
        workspace.path(),
        &[
            ("r1.txt", "c".repeat(400).into_bytes()),
            ("r2.txt", "c".repeat(400).into_bytes()),
            ("r3.txt", "c".repeat(400).into_bytes()),
            ("r4.txt", "c".repeat(400).into_bytes()),
            ("r5.txt", "c".repeat(400).into_bytes()),
        ],
    );
    // The summarising compaction can only fire on a transcript whose older
    // slice (outside the kept-8 tail) holds two conversational messages, and
    // a single `run` can never get there: every mid-run assistant message
    // carries tool calls, so `safe_split_point` cascades back over the whole
    // turn and `plan()` rejects the slice as degenerate. The one surface
    // that both prints through `stream_events` and can carry plain
    // assistant turns in its history is `resume` — so seed a session whose
    // transcript already holds two completed plain-text turns (hand-written
    // like cli_resume_surfaces does; no tool_registry_hash, which resume
    // downgrades to a warning), and let the resumed turn make five
    // 400-char reads. They land the transcript over the char threshold
    // (4800 — the stub reports zero usage so the token threshold never
    // decides) but under the hard budget (6500), and the split lands on the
    // seeded history: the summary request is script slot 5.
    let session_dir = home.path().join("sessions").join("seeded-compaction-245");
    std::fs::create_dir_all(&session_dir).expect("mkdir session dir");
    std::fs::write(
        session_dir.join(".meta.json"),
        serde_json::json!({
            "session_id": "seeded-compaction-245",
            "goal": "seeded history",
            "model": "test-model",
            "provider": "openai",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "message_count": 4,
        })
        .to_string(),
    )
    .expect("write meta");
    let mut lines = String::new();
    for (i, content) in [("m1", "first goal"), ("m3", "second goal")] {
        lines.push_str(&format!(
            "{{\"id\":\"{i}\",\"role\":\"user\",\"content\":\"{content}\",\"timestamp\":\"2026-01-01T00:00:00Z\"}}\n"
        ));
    }
    for i in ["m2", "m4"] {
        lines.push_str(&format!(
            "{{\"id\":\"{i}\",\"role\":\"assistant\",\"content\":\"earlier turn finished\",\"timestamp\":\"2026-01-01T00:00:00Z\"}}\n"
        ));
    }
    std::fs::write(session_dir.join("transcript.jsonl"), lines).expect("write transcript");

    let mut script = vec![];
    for n in 1..=5 {
        script.push(Reply::Tool {
            tool: "Read",
            arguments: read_args(&format!("r{n}.txt")),
        });
    }
    script.push(Reply::Text("summary of the reads".into()));
    script.push(Reply::Text("turn complete".into()));
    let stub = Stub::start(script);

    let mut cmd = run_child(
        home.path(),
        workspace.path(),
        &stub,
        &["--max-transcript-chars", "6500"],
        &[("RECURSIVE_COMPACT_THRESHOLD", "4800")],
    );
    cmd.arg("resume")
        .arg("--from-file")
        .arg(&session_dir)
        .arg("--message")
        .arg("now read the files");
    cmd.stdin(Stdio::null());
    let out = wait_with_timeout(
        cmd.spawn().expect("spawn recursive"),
        Duration::from_secs(120),
    );
    assert!(
        out.status.success(),
        "resume must exit 0: {}",
        stderr_of(&out)
    );
    let out = stdout_of(&out);
    assert!(
        out.lines()
            .any(|l| l.contains("compacted ") && l.contains("-char summary")),
        "compaction line missing: {out:?}"
    );
    assert!(
        out.lines()
            .any(|l| l.contains("steps] reason: no_more_tool_calls")),
        "{out:?}"
    );
}

#[test]
fn run_with_subagent_streams_attributed_worker_lines() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    // Parent asks for one worker; the worker's own LLM call is script slot 1.
    let stub = Stub::start(vec![
        Reply::Tool {
            tool: "agent",
            arguments: agent_args("helper"),
        },
        Reply::Text("worker says hi".into()),
        Reply::Text("all done".into()),
    ]);
    let mut cmd = run_child(home.path(), workspace.path(), &stub, &[], &[]);
    cmd.arg("run").arg("delegate to a worker");
    cmd.stdin(Stdio::null());
    let out = wait_with_timeout(
        cmd.spawn().expect("spawn recursive"),
        Duration::from_secs(120),
    );
    assert!(out.status.success(), "run must exit 0: {}", stderr_of(&out));
    let out = stdout_of(&out);
    assert!(
        out.lines()
            .any(|l| l.contains("[worker helper] worker says hi")),
        "attributed worker text missing: {out:?}"
    );
    assert!(
        out.lines()
            .any(|l| l.contains("[worker helper] done after")),
        "attributed worker finish line missing: {out:?}"
    );
    assert!(
        out.lines()
            .any(|l| l.contains("steps] reason: no_more_tool_calls")),
        "{out:?}"
    );
}

#[test]
fn repl_pipe_renders_bare_text_tool_hint_and_worker_lines() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    seed_files(workspace.path(), &[("small.txt", b"tiny".to_vec())]);
    // Goal 1: text + Read (bare repl text, stderr tool hint). Goal 2: worker
    // dispatch (attributed lines), then a whitespace-only final text that
    // must stay off the repl stream.
    let stub = Stub::start(vec![
        Reply::TextAndTool {
            content: "repl hi".into(),
            tool: "Read",
            arguments: read_args("small.txt"),
        },
        Reply::Text("repl done".into()),
        Reply::Tool {
            tool: "agent",
            arguments: agent_args("helper"),
        },
        Reply::Text("worker repl answer".into()),
        Reply::Text("   ".into()),
    ]);
    let mut cmd = run_child(home.path(), workspace.path(), &stub, &[], &[]);
    cmd.arg("repl");
    cmd.stdin(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn recursive");
    use std::io::Write as _;
    child
        .stdin
        .take()
        .expect("repl stdin")
        .write_all(b"look around\ndelegate to a worker\n")
        .expect("feed repl goals");
    let out = wait_with_timeout(child, Duration::from_secs(120));
    assert!(
        out.status.success(),
        "repl must exit 0: {}",
        stderr_of(&out)
    );
    let err = stderr_of(&out);
    let out = stdout_of(&out);
    assert!(out.lines().any(|l| l == "repl hi"), "{out:?}");
    assert!(out.lines().any(|l| l == "repl done"), "{out:?}");
    assert!(
        err.lines().any(|l| l.contains("↳ Read")),
        "repl tool hint missing on stderr: {err:?}"
    );
    assert!(
        out.lines()
            .any(|l| l.contains("[worker helper] worker repl answer")),
        "repl worker line missing: {out:?}"
    );
    assert!(
        !out.lines().any(|l| l == "   "),
        "blank assistant text must stay off the repl stream: {out:?}"
    );
}

#[test]
fn repl_json_mode_streams_raw_ndjson_events() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let stub = Stub::start(vec![Reply::Text("json turn".into())]);
    let mut cmd = run_child(
        home.path(),
        workspace.path(),
        &stub,
        &["--output-format", "json"],
        &[],
    );
    cmd.arg("repl");
    cmd.stdin(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn recursive");
    use std::io::Write as _;
    child
        .stdin
        .take()
        .expect("repl stdin")
        .write_all(b"say something\n")
        .expect("feed repl goal");
    let out = wait_with_timeout(child, Duration::from_secs(120));
    assert!(
        out.status.success(),
        "repl must exit 0: {}",
        stderr_of(&out)
    );
    let out = stdout_of(&out);
    assert!(
        out.contains("\"type\":\"assistant_text\""),
        "raw AgentEvent NDJSON missing: {out:?}"
    );
    assert!(out.contains("json turn"), "{out:?}");
}
