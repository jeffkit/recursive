//! Black-box pin for the `acp` subcommand's server loop (`AcpServer::run`).
//!
//! Nothing inside the recursive-agent test suite calls `AcpServer::run` —
//! its only caller is `Cmd::Acp` in `src/main.rs` — so a body-void mutant
//! of that function is invisible to every in-process test (it was caught
//! by no scan either; #245 batch 32 found it MISSED). The pin drives the
//! real binary over real stdio pipes:
//!
//! * while stdin is held open, the server must stay up (a void `run`
//!   returns immediately and the process exits before any input);
//! * an `initialize` request must be answered on stdout (the loop is
//!   actually wired to `run_stdio`);
//! * EOF on stdin must shut the server down cleanly (exit success).
//!
//! Kills `src/acp/server.rs:1862:9 replace AcpServer::run with ()`.
//!
//! Unix-only: the fixture relies on the empty-`$HOME`/`RECURSIVE_HOME`
//! env-only config bootstrap the other cli surface tests use; windows
//! config discovery (USERPROFILE) is unexercised — unlock by verifying
//! that bootstrap on windows and dropping this gate (see
//! cli_signal_surfaces.rs / turn_mutants.rs for the unix-only precedent).
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[test]
fn acp_server_serves_initialize_over_stdio_and_exits_on_eof() {
    let home = tempfile::TempDir::new().expect("temp home");
    let mut child = Command::new(env!("CARGO_BIN_EXE_recursive"))
        .arg("acp")
        .env("HOME", home.path())
        .env("RECURSIVE_HOME", home.path())
        .env("RECURSIVE_API_KEY", "acp-surface-test-key")
        .env("RECURSIVE_RETRY_MAX", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn recursive acp");

    // Drain stderr in the background so a chatty child can't wedge on a
    // full pipe, and keep a tail for failure diagnostics.
    let stderr_tail = Arc::new(Mutex::new(String::new()));
    drain_into(
        child.stderr.take().expect("child stderr"),
        stderr_tail.clone(),
    );

    // stdout lines become channel messages.
    let (tx, rx) = mpsc::channel::<String>();
    drain_into_lines(stdout_from(&mut child), tx);

    // While stdin is held open the server must keep serving: a void run()
    // would let the process exit right away.
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        child.try_wait().expect("poll child").is_none(),
        "acp server exited while stdin was still open\nstderr tail:\n{}",
        stderr_tail.lock().expect("stderr tail mutex")
    );

    // The loop is wired: an initialize request is answered on stdout.
    let stdin = child.stdin.as_mut().expect("child stdin");
    stdin
        .write_all(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
        )
        .expect("write initialize request");
    stdin.write_all(b"\n").expect("newline-terminate request");
    stdin.flush().expect("flush initialize request");

    let deadline = Instant::now() + Duration::from_secs(30);
    let response = loop {
        let remain = deadline.saturating_duration_since(Instant::now());
        assert!(
            remain > Duration::ZERO,
            "no initialize response within 30s\nstderr tail:\n{}",
            stderr_tail.lock().expect("stderr tail mutex")
        );
        match rx.recv_timeout(remain) {
            Ok(line) => {
                let v: serde_json::Value =
                    serde_json::from_str(line.trim()).unwrap_or(serde_json::Value::Null);
                if v.is_object() && v.get("id") == Some(&serde_json::json!(1)) {
                    break v;
                }
                // Not our response (banner/notification): keep waiting.
            }
            Err(mpsc::RecvTimeoutError::Timeout) => panic!(
                "timed out waiting for the initialize response\nstderr tail:\n{}",
                stderr_tail.lock().expect("stderr tail mutex")
            ),
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!(
                "stdout closed before the initialize response arrived\nstderr tail:\n{}",
                stderr_tail.lock().expect("stderr tail mutex")
            ),
        }
    };
    assert_eq!(
        response["jsonrpc"], "2.0",
        "initialize response: {response}"
    );
    assert!(
        response["result"].is_object(),
        "initialize must succeed: {response}"
    );

    // EOF on stdin shuts the server down cleanly (#183).
    drop(child.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match child.try_wait().expect("poll child") {
            Some(status) => {
                assert!(
                    status.success(),
                    "acp server exit status after stdin EOF: {status}\nstderr tail:\n{}",
                    stderr_tail.lock().expect("stderr tail mutex")
                );
                break;
            }
            None => {
                assert!(
                    Instant::now() < deadline,
                    "acp server did not exit after stdin EOF\nstderr tail:\n{}",
                    stderr_tail.lock().expect("stderr tail mutex")
                );
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn stdout_from(child: &mut Child) -> std::process::ChildStdout {
    child.stdout.take().expect("child stdout")
}

/// Pump a byte stream into the shared diagnostics tail, line by line.
fn drain_into<R: std::io::Read + Send + 'static>(
    reader: R,
    tail: Arc<Mutex<String>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let mut t = tail.lock().expect("stderr tail mutex");
                    if t.len() > 4000 {
                        t.clear();
                    }
                    t.push_str(&line);
                }
            }
        }
    })
}

/// Pump newline-delimited lines into a channel; the channel closing (test
/// thread gone) or EOF ends the pump.
fn drain_into_lines<R: std::io::Read + Send + 'static>(
    reader: R,
    tx: mpsc::Sender<String>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.send(line.clone()).is_err() {
                        break;
                    }
                }
            }
        }
    })
}
