//! Black-box spawn tests for `recursive`'s *command* surfaces: `doctor`,
//! `mcp`, `update`, `agents`, `providers`, and the `serve` MCP-stdio
//! dispatcher.
//!
//! Every test spawns the real binary in a hermetic `RECURSIVE_HOME` +
//! workspace and asserts on stdout / stderr / exit code. That is deliberate:
//! the mutants these tests kill live in `main.rs`'s command functions, and
//! the only way to observe "the function body was replaced with `Ok(())`",
//! "this match arm was deleted", or "this `!` was dropped" is to run the
//! command and look at what it actually printed / returned.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// A hermetic CLI environment: an isolated `RECURSIVE_HOME`, workspace, and
/// sessions directory so a test never reads (or writes) the developer's real
/// config, sessions, or provider cache.
struct Cli {
    home: tempfile::TempDir,
    workspace: tempfile::TempDir,
    sessions: tempfile::TempDir,
}

impl Cli {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().expect("tempdir"),
            workspace: tempfile::tempdir().expect("tempdir"),
            sessions: tempfile::tempdir().expect("tempdir"),
        }
    }

    fn workspace_path(&self) -> &Path {
        self.workspace.path()
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_recursive"));
        c.env("RECURSIVE_HOME", self.home.path());
        c.env("RECURSIVE_WORKSPACE", self.workspace.path());
        c.env("RECURSIVE_SESSIONS_DIR", self.sessions.path());
        // Prove the tests are hermetic: the ambient shell (and CI) may carry
        // real provider keys / model overrides, which would flip `doctor`'s
        // API-key check and change which preset is active.
        for var in [
            "RECURSIVE_API_KEY",
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "DEEPSEEK_API_KEY",
            "MINIMAX_API_KEY",
            "GLM_API_KEY",
            "RECURSIVE_MODEL",
            "RECURSIVE_API_BASE",
            "RECURSIVE_PROVIDER_TYPE",
            "RECURSIVE_HEADLESS",
            "RECURSIVE_MCP_CONFIG",
            "RECURSIVE_PROVIDERS_URL",
            "RECURSIVE_PROVIDERS_AUTO_REFRESH",
            "RECURSIVE_UPDATE_URL",
            // Environment proxies must never intercept the loopback stubs.
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "http_proxy",
            "https_proxy",
        ] {
            c.env_remove(var);
        }
        c.env("NO_PROXY", "127.0.0.1,localhost");
        c.env("no_proxy", "127.0.0.1,localhost");
        c
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.cmd().args(args).output().expect("spawn recursive")
    }
}

fn stdout_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

// ─── doctor ──────────────────────────────────────────────────────────────────

/// `cmd_doctor -> Ok(())`: the replacement body returns immediately, printing
/// nothing and exiting 0. The real body prints the diagnostics banner and
/// exits 1 when a check fails.
#[test]
fn doctor_reports_failing_check_and_exits_1() {
    let cli = Cli::new();
    let out = cli.run(&["doctor"]);
    assert_eq!(out.status.code(), Some(1), "doctor should fail: {out:?}");
    assert!(
        stdout_of(&out).contains("Recursive diagnostics"),
        "missing diagnostics banner: {out:?}"
    );
    assert!(
        stderr_of(&out).contains("One or more checks failed."),
        "missing failure banner: {out:?}"
    );
}

/// `Ok(servers) if !servers.is_empty()`: with a configured server the arm must
/// be taken (kills both `guard -> false` and the dropped `!`).
#[test]
fn doctor_autodiscovers_workspace_mcp_servers() {
    let cli = Cli::new();
    std::fs::write(
        cli.workspace_path().join(".mcp.json"),
        r#"{"mcpServers":{"demo":{"command":"echo"}}}"#,
    )
    .expect("write .mcp.json");
    let out = cli.run(&["doctor"]);
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("Auto-discovered 1 MCP server(s)"),
        "auto-discovery not reported: {stdout}"
    );
}

/// ... and with *no* server the `Ok(_)` arm must run (kills `guard -> true`,
/// which would report "Auto-discovered 0 MCP server(s)").
#[test]
fn doctor_reports_no_mcp_servers_when_none_configured() {
    let cli = Cli::new();
    let out = cli.run(&["doctor"]);
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("No MCP servers configured (optional)"),
        "empty discovery not reported: {stdout}"
    );
}

// ─── mcp ─────────────────────────────────────────────────────────────────────

/// `cmd_mcp -> Ok(())`: the real body prints the "none configured" notice.
#[test]
fn mcp_list_reports_none_configured() {
    let cli = Cli::new();
    let out = cli.run(&["mcp", "list"]);
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("No MCP servers configured."),
        "mcp list produced no notice: {stdout}"
    );
}

/// `starts_with("http://") || starts_with("https://")`: an `https://` value
/// must be stored as a `url` entry, not a `command` (kills the `|| -> &&`).
#[test]
fn mcp_add_stores_https_server_as_url() {
    let cli = Cli::new();
    let out = cli.run(&["mcp", "add", "demo", "https://example.com/mcp"]);
    assert!(out.status.success(), "mcp add failed: {out:?}");
    let body = std::fs::read_to_string(cli.workspace_path().join(".mcp.json")).expect("read json");
    assert!(
        body.contains(r#""url": "https://example.com/mcp""#),
        "https add was not stored as a url entry: {body}"
    );
}

#[test]
fn mcp_add_then_remove_roundtrips() {
    let cli = Cli::new();
    assert!(
        cli.run(&["mcp", "add", "srv", "npx", "server"])
            .status
            .success(),
        "add should succeed"
    );
    let out = cli.run(&["mcp", "remove", "srv"]);
    assert!(out.status.success(), "remove should succeed: {out:?}");
    let body = std::fs::read_to_string(cli.workspace_path().join(".mcp.json")).expect("read json");
    assert!(!body.contains("srv"), "server not removed: {body}");
}

/// `if !mcp_json.exists()`: removing from a missing file must bail with the
/// dedicated message (kills the dropped `!`, which would instead surface a
/// raw `read_to_string` I/O error).
#[test]
fn mcp_remove_without_config_file_errors() {
    let cli = Cli::new();
    let out = cli.run(&["mcp", "remove", "ghost"]);
    assert_eq!(out.status.code(), Some(1), "remove should fail: {out:?}");
    assert!(
        stderr_of(&out).contains(".mcp.json not found"),
        "missing dedicated error: {out:?}"
    );
}

// ─── agents ──────────────────────────────────────────────────────────────────

/// `cmd_agents -> Ok(())`: the real body prints the "no active sessions" hint.
#[test]
fn agents_reports_no_active_sessions() {
    let cli = Cli::new();
    let out = cli.run(&["agents"]);
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("No active agent sessions."),
        "agents printed nothing: {stdout}"
    );
}

/// `meta.status == SessionStatus::Active`: an active session must be listed
/// (kills the `== -> !=`, which would filter it out).
#[test]
fn agents_lists_active_session() {
    let cli = Cli::new();
    let session_dir = cli
        .sessions
        .path()
        .join("slug")
        .join("2026-01-01T000000-sess");
    std::fs::create_dir_all(&session_dir).expect("mkdir session");
    std::fs::write(
        session_dir.join(".meta.json"),
        r#"{
            "session_id": "sess-xyz",
            "goal": "do the thing",
            "model": "m",
            "provider": "openai",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "message_count": 0,
            "status": "active"
        }"#,
    )
    .expect("write meta");

    let out = cli.run(&["agents"]);
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("sess-xyz"),
        "active session not listed: {stdout}"
    );
}

// ─── providers ───────────────────────────────────────────────────────────────

/// `cmd_providers -> Ok(())`: `providers list` must print the catalog table.
#[test]
fn providers_list_prints_effective_presets() {
    let cli = Cli::new();
    let out = cli.run(&["providers", "list"]);
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("DEFAULT_MODEL"),
        "no catalog header: {stdout}"
    );
    assert!(stdout.contains("deepseek"), "no bundled preset: {stdout}");
}

/// `presets.iter().find(|p| p.id == id)`: requesting `deepseek` must print
/// *its* models (kills `== -> !=`, which would return the first preset whose
/// id differs — `anthropic`).
#[test]
fn providers_models_resolves_requested_preset() {
    let cli = Cli::new();
    let out = cli.run(&["providers", "models", "deepseek"]);
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("deepseek-v4-pro"),
        "requested preset's models not printed: {stdout}"
    );
}

/// `if !path.exists()`: with no cache the status command must report "no
/// cache" (kills the dropped `!`, which would fall through to "Age: unknown").
#[test]
fn providers_status_reports_missing_cache() {
    let cli = Cli::new();
    let out = cli.run(&["providers", "status"]);
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("no cache"),
        "missing-cache notice absent: {stdout}"
    );
}

// ─── update ──────────────────────────────────────────────────────────────────

/// Spin a one-shot loopback HTTP stub and return its URL. The stub answers the
/// first request with `status` / `body`, then exits. Detached: the client is
/// the only thing that will ever connect.
fn http_stub(status: &str, body: &str) -> String {
    use std::io::Read;
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
    listener.set_nonblocking(true).expect("nonblocking");
    let addr = listener.local_addr().expect("addr");
    let status = status.to_string();
    let body = body.to_string();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            // Never bail on a transient accept error (EINTR / ECONNABORTED):
            // dropping the listener would close the port the client is about
            // to connect to.
            if Instant::now() > deadline {
                return;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    // Accepted sockets inherit non-blocking mode from the
                    // listener on macOS/BSD; force blocking so we fully read
                    // the request before replying (otherwise we answer and
                    // close while the client is still writing → "error
                    // sending request").
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let mut buf = [0u8; 8192];
                    let _ = stream.read(&mut buf);
                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.flush();
                    return;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    });
    format!("http://{addr}/releases/latest")
}

/// `Ok(r) if r.status().is_success()` (guard -> false): a 200 release newer
/// than the running version must be reported (the mutant would route it to the
/// non-success arm and print "GitHub API returned status 200 ...").
#[test]
fn update_reports_new_version_when_release_is_newer() {
    let cli = Cli::new();
    let url = http_stub(
        "200 OK",
        r#"{"tag_name":"v999.0.0","html_url":"https://example.com/r"}"#,
    );
    let out = cli
        .cmd()
        .env("RECURSIVE_UPDATE_URL", &url)
        .arg("update")
        .output()
        .expect("spawn update");
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("New version available: v999.0.0"),
        "newer release not reported: {stdout}"
    );
}

/// `latest == current`: a release whose tag equals the running version must be
/// reported as up-to-date (kills `== -> !=`).
#[test]
fn update_reports_up_to_date_on_matching_version() {
    let cli = Cli::new();
    let current = env!("CARGO_PKG_VERSION");
    let body = format!(r#"{{"tag_name":"v{current}","html_url":"https://example.com/r"}}"#);
    let url = http_stub("200 OK", &body);
    let out = cli
        .cmd()
        .env("RECURSIVE_UPDATE_URL", &url)
        .arg("update")
        .output()
        .expect("spawn update");
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("You are on the latest version."),
        "matching version not reported as latest: {stdout}"
    );
}

/// `Ok(r) if r.status().is_success()` (guard -> true): a non-2xx response must
/// go to the status-reporting arm (the mutant would try to parse it as JSON and
/// print "Could not parse release info.").
#[test]
fn update_reports_non_success_status() {
    let cli = Cli::new();
    let url = http_stub("500 Internal Server Error", "boom");
    let out = cli
        .cmd()
        .env("RECURSIVE_UPDATE_URL", &url)
        .arg("update")
        .output()
        .expect("spawn update");
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("GitHub API returned status 500"),
        "non-success status not reported: {stdout}"
    );
}

// ─── serve (MCP stdio dispatcher) ────────────────────────────────────────────

/// Feed newline-delimited JSON-RPC to `recursive serve` on stdin, close stdin,
/// and collect the process output. Responses are printed one-per-line on
/// stdout; diagnostics go to stderr.
fn run_serve(cli: &Cli, lines: &[&str]) -> std::process::Output {
    let mut child = cli
        .cmd()
        .arg("serve")
        .arg("--workspace")
        .arg(cli.workspace_path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn serve");
    {
        let mut stdin = child.stdin.take().expect("stdin");
        for line in lines {
            writeln!(stdin, "{line}").expect("write request");
        }
        // stdin dropped here → server sees EOF and exits.
    }
    child.wait_with_output().expect("wait serve")
}

/// One JSON-RPC round-trip helper: send `line`, return stdout.
fn serve_once(cli: &Cli, line: &str) -> String {
    stdout_of(&run_serve(cli, &[line]))
}

/// Kills `run_mcp_server_stdio -> Ok(())` (no `initialize` response at all),
/// `dispatch_request_via_registry -> None` (no response at all), and the
/// deleted `"initialize"` match arm (which would fall through to
/// `method_not_found`).
#[test]
fn serve_initialize_responds_with_server_info() {
    let cli = Cli::new();
    let stdout = serve_once(&cli, r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#);
    assert!(stdout.contains("serverInfo"), "no serverInfo: {stdout}");
    assert!(
        stdout.contains("recursive-agent"),
        "no server name: {stdout}"
    );
}

/// Deleted `"tools/list"` arm → `method_not_found` instead of a tool array.
#[test]
fn serve_tools_list_returns_tool_specs() {
    let cli = Cli::new();
    let stdout = serve_once(&cli, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
    assert!(stdout.contains(r#""tools""#), "no tools array: {stdout}");
    assert!(stdout.contains("inputSchema"), "no tool schema: {stdout}");
}

/// Deleted `"tools/call"` arm → `method_not_found` instead of an `isError`
/// tool result.
#[test]
fn serve_tools_call_reports_error_result_for_unknown_tool() {
    let cli = Cli::new();
    let stdout = serve_once(
        &cli,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"no_such_tool","arguments":{}}}"#,
    );
    assert!(
        stdout.contains(r#""isError":true"#),
        "no isError result: {stdout}"
    );
}

/// Deleted `"resources/list"` arm → `method_not_found` instead of `[]`.
#[test]
fn serve_resources_list_returns_empty_array() {
    let cli = Cli::new();
    let stdout = serve_once(
        &cli,
        r#"{"jsonrpc":"2.0","id":3,"method":"resources/list"}"#,
    );
    assert!(
        stdout.contains(r#""resources":[]"#),
        "no empty list: {stdout}"
    );
}

/// Deleted `"resources/read"` arm (message would become "Method not found")
/// *and* the `delete -` on its `-32601` code.
#[test]
fn serve_resources_read_is_not_supported() {
    let cli = Cli::new();
    let stdout = serve_once(
        &cli,
        r#"{"jsonrpc":"2.0","id":3,"method":"resources/read"}"#,
    );
    assert!(
        stdout.contains(r#""code":-32601"#),
        "wrong error code: {stdout}"
    );
    assert!(
        stdout.contains("resources/read not supported"),
        "handled by the fallback instead of its own arm: {stdout}"
    );
}

/// Deleted `"prompts/list"` arm → `method_not_found` instead of `[]`.
#[test]
fn serve_prompts_list_returns_empty_array() {
    let cli = Cli::new();
    let stdout = serve_once(&cli, r#"{"jsonrpc":"2.0","id":6,"method":"prompts/list"}"#);
    assert!(
        stdout.contains(r#""prompts":[]"#),
        "no empty list: {stdout}"
    );
}

/// Deleted `"prompts/get"` arm (message would become "Method not found")
/// *and* the `delete -` on its `-32601` code.
#[test]
fn serve_prompts_get_is_not_supported() {
    let cli = Cli::new();
    let stdout = serve_once(&cli, r#"{"jsonrpc":"2.0","id":7,"method":"prompts/get"}"#);
    assert!(
        stdout.contains(r#""code":-32601"#),
        "wrong error code: {stdout}"
    );
    assert!(
        stdout.contains("prompts/get not supported"),
        "handled by the fallback instead of its own arm: {stdout}"
    );
}

/// The fallback arm still reports an unknown method as `method_not_found`.
#[test]
fn serve_unknown_method_is_method_not_found() {
    let cli = Cli::new();
    let stdout = serve_once(&cli, r#"{"jsonrpc":"2.0","id":8,"method":"totally/bogus"}"#);
    assert!(
        stdout.contains("Method not found: totally/bogus"),
        "stdout: {stdout}"
    );
}

/// Deleted `"notifications/initialized"` arm: unlike the `_` fallback it
/// answers with *nothing* even when the notification carries an id.
#[test]
fn serve_notification_with_id_gets_no_response() {
    let cli = Cli::new();
    let out = run_serve(
        &cli,
        &[r#"{"jsonrpc":"2.0","id":5,"method":"notifications/initialized"}"#],
    );
    assert!(
        stdout_of(&out).trim().is_empty(),
        "notification should not be answered: {:?}",
        stdout_of(&out)
    );
}

/// `-32700`: an unparseable line must be rejected with the JSON-RPC parse-error
/// code (kills the `delete -`, which would emit `32700`).
#[test]
fn serve_parse_error_uses_negative_code() {
    let cli = Cli::new();
    let stdout = serve_once(&cli, r#"{"jsonrpc":"2.0","id":"#);
    assert!(
        stdout.contains(r#""code":-32700"#),
        "wrong parse-error code: {stdout}"
    );
}
