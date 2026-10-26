//! SIGTERM surfaces of the single-shot `recursive run` (`cli::interrupt`).
//!
//! Pins two contracts that only exist when the real binary receives a real
//! signal (issue #245 batch 2):
//!
//! * a run interrupted by SIGTERM exits with `128 + 15` — the conventional
//!   "killed by SIGTERM" status — so a supervisor that sent the signal sees
//!   the run did not finish its goal (`TERM_EXIT_CODE`);
//! * a run parked inside a long tool execution (which the shutdown token
//!   cannot interrupt) is force-exited once the drain grace elapses, instead
//!   of waiting for the tool (`spawn_term_watchdog`, `TERM_DRAIN_GRACE_SECS`).
//!
//! Unix-only: SIGTERM delivery and the 128+signal convention are POSIX.
#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// `128 + SIGTERM(15)`: the status `cli::interrupt::TERM_EXIT_CODE` encodes.
const TERM_EXIT_CODE: i32 = 143;

/// What the stub provider does with the request it just read.
#[derive(Clone)]
enum Reply {
    /// Never answer: the run stays parked inside `provider.complete()`.
    Hold,
    /// Ask for one `Bash` tool call with this command.
    Bash(String),
    /// A final text turn.
    Text,
}

/// Minimal fake OpenAI endpoint. The first request gets `first`, every later
/// one a final text turn; each request read is reported on the channel so the
/// test can time the signal.
struct Stub {
    addr: SocketAddr,
    requests: mpsc::Receiver<()>,
}

impl Stub {
    fn start(first: Reply) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
        let addr = listener.local_addr().expect("stub addr");
        let (tx, requests) = mpsc::channel();
        std::thread::spawn(move || {
            for (n, stream) in listener.incoming().enumerate() {
                let Ok(stream) = stream else { break };
                let reply = if n == 0 { first.clone() } else { Reply::Text };
                let tx = tx.clone();
                std::thread::spawn(move || serve(stream, reply, tx));
            }
        });
        Self { addr, requests }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn wait_for_request(&self, limit: Duration) {
        self.requests
            .recv_timeout(limit)
            .expect("the run must reach the provider");
    }
}

fn read_request(stream: &mut TcpStream) {
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
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
}

fn serve(mut stream: TcpStream, reply: Reply, tx: mpsc::Sender<()>) {
    read_request(&mut stream);
    let _ = tx.send(());
    let body = match reply {
        Reply::Hold => {
            // Park long past the test's own ceilings; the child is gone (and
            // the socket reset) well before this elapses.
            std::thread::sleep(Duration::from_secs(120));
            return;
        }
        Reply::Bash(command) => {
            let arguments = serde_json::json!({ "command": command }).to_string();
            serde_json::json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": { "name": "Bash", "arguments": arguments }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": { "prompt_tokens": 3, "completion_tokens": 5, "total_tokens": 8 }
            })
            .to_string()
        }
        Reply::Text => serde_json::json!({
            "choices": [{
                "message": { "role": "assistant", "content": "done" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 3, "completion_tokens": 5, "total_tokens": 8 }
        })
        .to_string(),
    };
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.flush();
}

struct Rig {
    home: tempfile::TempDir,
    workspace: tempfile::TempDir,
    stub: Stub,
}

impl Rig {
    fn new(first: Reply) -> Self {
        Self {
            home: tempfile::tempdir().expect("home tempdir"),
            workspace: tempfile::tempdir().expect("workspace tempdir"),
            stub: Stub::start(first),
        }
    }

    /// `recursive run <goal>` against the stub, tools auto-approved (the
    /// watchdog case needs `Bash` to actually execute).
    fn spawn_run(&self) -> Child {
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
        cmd.arg("--permission-mode").arg("auto");
        cmd.arg("run").arg("exercise the shutdown path");
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.spawn().expect("spawn recursive")
    }
}

fn sigterm(child: &Child) {
    // SAFETY: plain signal delivery to a pid this test spawned and has not
    // reaped yet, so it cannot have been recycled.
    let rc = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    assert_eq!(
        rc,
        0,
        "SIGTERM delivery failed: {}",
        std::io::Error::last_os_error()
    );
}

/// `wait_with_output` with a hard ceiling: a run that never exits is
/// SIGKILLed so the test fails instead of hanging the binary.
fn wait_with_timeout(child: Child, limit: Duration) -> Output {
    let pid = child.id() as libc::pid_t;
    let done = Arc::new(AtomicBool::new(false));
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
            // SAFETY: see `sigterm` — the pid is still unreaped here.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    });
    let out = child.wait_with_output().expect("wait for recursive");
    done.store(true, Ordering::SeqCst);
    out
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn wait_for_file(path: &Path, limit: Duration) {
    let deadline = Instant::now() + limit;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "the tool never started (no {} after {limit:?})",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A run parked in `provider.complete()` is cancelled by SIGTERM at once
/// (the kernel selects the shutdown token against the call) and the process
/// then exits with `128 + 15`, not 0 and not some other arithmetic on 128.
#[test]
fn run_interrupted_by_sigterm_exits_with_128_plus_15() {
    let rig = Rig::new(Reply::Hold);
    let child = rig.spawn_run();
    rig.stub.wait_for_request(Duration::from_secs(60));

    sigterm(&child);
    let out = wait_with_timeout(child, Duration::from_secs(60));
    let stderr = stderr_of(&out);

    assert_eq!(
        out.status.code(),
        Some(TERM_EXIT_CODE),
        "a SIGTERM-interrupted run must exit 128+15, got {:?}; stderr:\n{stderr}",
        out.status
    );
    assert!(
        !stderr.contains("forcing exit"),
        "a parked provider call drains at once — the grace watchdog must not be what ended it:\n{stderr}"
    );
}

/// A run parked inside a tool execution cannot be interrupted by the token;
/// the watchdog force-exits it once the 10s drain grace elapses — with the
/// SIGTERM status and the grace announced — instead of waiting out the tool.
/// Without the watchdog the process would only exit when `sleep` returned
/// (25s here) and would never print the grace line.
#[test]
fn run_parked_in_a_tool_is_force_exited_after_the_drain_grace() {
    let workspace = tempfile::tempdir().expect("marker tempdir");
    let marker = workspace.path().join("tool-started");
    let rig = Rig::new(Reply::Bash(format!(
        "touch '{}' && sleep 25",
        marker.display()
    )));
    let child = rig.spawn_run();
    rig.stub.wait_for_request(Duration::from_secs(60));
    wait_for_file(&marker, Duration::from_secs(60));

    let signalled = Instant::now();
    sigterm(&child);
    let out = wait_with_timeout(child, Duration::from_secs(60));
    let elapsed = signalled.elapsed();
    let stderr = stderr_of(&out);

    assert_eq!(
        out.status.code(),
        Some(TERM_EXIT_CODE),
        "the forced exit must carry the SIGTERM status, got {:?}; stderr:\n{stderr}",
        out.status
    );
    assert!(
        stderr.contains("drain grace period (10s) elapsed; forcing exit"),
        "the watchdog must announce the forced exit, got:\n{stderr}"
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "the watchdog must fire at the 10s grace, not wait for the 25s tool: took {elapsed:?}"
    );
}
