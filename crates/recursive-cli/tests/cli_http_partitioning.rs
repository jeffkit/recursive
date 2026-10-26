//! `recursive http` startup wiring of the per-session partitioning inputs
//! (Goal 431 / #244 W2a+W2b), observed through the real binary.
//!
//! `main.rs` hands the serve entry three raw inputs — the endpoint registry
//! discovered from the workspace, the skill-source URL templates read from
//! the environment and the directory-only skill catalog — through
//! `recursive::http::SessionPartitioning`. The library tests pin what the
//! server does with them; these tests pin that the binary actually passes
//! them in (issue #245 batch 2):
//!
//! * a templated endpoint header is announced at startup (the
//!   `endpoint headers with {{...}} templates` WARN) and makes session
//!   creation fail closed (503) for a session that carries no labels;
//! * a templated skill-source URL makes session creation fail closed the
//!   same way, and a registry without templates does *not* trigger the WARN.
//!
//! Each test starts a server on a free loopback port, drives it with a raw
//! HTTP/1.1 request, then kills it and reads the startup stderr.
//!
//! Unix-only: the server is torn down with a signal.
#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Rig {
    home: tempfile::TempDir,
    workspace: tempfile::TempDir,
}

impl Rig {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().expect("home tempdir"),
            workspace: tempfile::tempdir().expect("workspace tempdir"),
        }
    }

    fn workspace_path(&self) -> &Path {
        self.workspace.path()
    }

    /// `<workspace>/.recursive/endpoints.json` with one endpoint carrying the
    /// given static headers.
    fn write_endpoints(&self, headers: serde_json::Value) {
        let dir = self.workspace_path().join(".recursive");
        std::fs::create_dir_all(&dir).expect("mkdir .recursive");
        let registry = serde_json::json!({
            "endpoints": {
                "ep": {
                    "method": "GET",
                    "base_url": "https://api.example.com",
                    "path": "/x",
                    "headers": headers
                }
            }
        });
        std::fs::write(
            dir.join("endpoints.json"),
            serde_json::to_vec_pretty(&registry).expect("endpoints json"),
        )
        .expect("write endpoints.json");
    }

    /// Spawn `recursive http` on a free loopback port. `extra_env` rides on
    /// top of the hermetic environment.
    fn spawn_http(&self, extra_env: &[(&str, &str)]) -> (Child, SocketAddr) {
        let probe = TcpListener::bind("127.0.0.1:0").expect("probe a free port");
        let addr = probe.local_addr().expect("probe addr");
        drop(probe);

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_recursive"));
        cmd.env_clear();
        cmd.env("HOME", self.home.path());
        cmd.env("RECURSIVE_HOME", self.home.path());
        cmd.env("RECURSIVE_RETRY_MAX", "0");
        cmd.env("RECURSIVE_HTTP_AUTH_KEYS", API_KEY);
        if let Some(path) = std::env::var_os("PATH") {
            cmd.env("PATH", path);
        }
        for (key, value) in extra_env {
            cmd.env(key, value);
        }
        cmd.current_dir(self.workspace_path());
        cmd.arg("--log").arg("error");
        cmd.arg("--workspace").arg(self.workspace_path());
        cmd.arg("--api-base").arg("http://127.0.0.1:1");
        cmd.arg("--api-key").arg("sk-test-key");
        cmd.arg("--model").arg("test-model");
        cmd.arg("--provider").arg("openai");
        cmd.arg("http").arg("--addr").arg(addr.to_string());
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let child = cmd.spawn().expect("spawn recursive http");
        (child, addr)
    }
}

/// Block until the server accepts connections (or the child died / the
/// ceiling passed).
fn wait_for_listener(child: &mut Child, addr: SocketAddr, limit: Duration) {
    let deadline = Instant::now() + limit;
    loop {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            return;
        }
        if let Some(status) = child.try_wait().expect("poll child") {
            panic!("recursive http exited before listening: {status:?}");
        }
        assert!(
            Instant::now() < deadline,
            "recursive http did not start listening on {addr} within {limit:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Inbound API key for the server under test. An API-key identity carries no
/// tenant (only a JWT `tenant` claim does), so the session it creates has no
/// labels — exactly the fail-closed case the partitioning templates guard.
const API_KEY: &str = "test-key-245";

/// One raw HTTP/1.1 request (authenticated); returns `(status_code, body)`.
fn request(addr: SocketAddr, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).expect("connect to recursive http");
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .expect("read timeout");
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nX-API-Key: {API_KEY}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .expect("write request");
    stream.flush().expect("flush request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response");
    let text = String::from_utf8_lossy(&raw).to_string();
    let status = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("malformed response:\n{text}"));
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

/// Kill the server and collect what it wrote to stderr before that.
fn stop(mut child: Child) -> Output {
    child.kill().expect("kill recursive http");
    child.wait_with_output().expect("reap recursive http")
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

const TEMPLATE_WARN: &str = "endpoint headers with {{...}} templates expand per session";

/// Templated endpoint header → announced at startup, and a session that
/// carries no labels cannot be created (the per-session header expansion
/// fails closed). A server that dropped the registry on the floor would
/// create the session happily with the raw template still in the header.
#[test]
fn http_templated_endpoint_header_warns_at_startup_and_fails_closed_for_an_unlabelled_session() {
    let rig = Rig::new();
    rig.write_endpoints(serde_json::json!({ "X-Tenant": "{tenant}" }));
    let (mut child, addr) = rig.spawn_http(&[]);
    wait_for_listener(&mut child, addr, Duration::from_secs(60));

    let (status, body) = request(addr, "POST", "/sessions", "{}");
    let out = stop(child);
    let stderr = stderr_of(&out);

    assert_eq!(
        status, 503,
        "an unlabelled session must fail closed on a templated header, got {status}: {body}\nstderr:\n{stderr}"
    );
    assert!(
        body.contains("endpoint headers unavailable for this session"),
        "the 503 must name the endpoint-header expansion, got: {body}"
    );
    assert!(
        stderr.contains(TEMPLATE_WARN) && stderr.contains("ep → X-Tenant"),
        "startup must announce the templated header (endpoint → header), got:\n{stderr}"
    );
}

/// Templated skill-source URL → a session that carries no labels cannot be
/// created (the template cannot be expanded; fail-closed), while a registry
/// whose headers carry no template must *not* raise the template WARN.
#[test]
fn http_templated_skill_source_fails_closed_and_plain_endpoint_headers_do_not_warn() {
    let rig = Rig::new();
    rig.write_endpoints(serde_json::json!({ "X-Static": "plain-value" }));
    let template = "https://skills.example.invalid/{tenant}/index.json";
    let (mut child, addr) = rig.spawn_http(&[("RECURSIVE_SKILL_SOURCE_URL", template)]);
    wait_for_listener(&mut child, addr, Duration::from_secs(60));

    let (status, body) = request(addr, "POST", "/sessions", "{}");
    let out = stop(child);
    let stderr = stderr_of(&out);

    assert_eq!(
        status, 503,
        "an unlabelled session must fail closed on a templated skill source, got {status}: {body}\nstderr:\n{stderr}"
    );
    assert!(
        body.contains("cannot be resolved for this session") && body.contains(template),
        "the 503 must name the operator template, got: {body}"
    );
    assert!(
        !stderr.contains(TEMPLATE_WARN),
        "a registry without templated headers must not raise the template WARN, got:\n{stderr}"
    );
}
