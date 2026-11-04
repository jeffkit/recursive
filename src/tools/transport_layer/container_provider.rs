//! Container-tier [`ToolSetProvider`] (Goal 403).
//!
//! [`ContainerToolSetProvider`] builds the full standard tool set (via
//! [`crate::tools::build_standard_tools_with_transport`]) with the single
//! shared transport (Goal 401) swapped for a [`ContainerTransport`], so
//! **every** I/O tool (Read / Write / Edit / Glob / Grep / count_lines /
//! Bash) executes inside a hardened, long-lived Docker container with the
//! workspace bind-mounted at `/workspace`.
//!
//! Unlike [`super::docker_provider::DockerToolSetProvider`] (which only
//! replaces the Bash tool), this rebinds the whole transport layer — and,
//! because it reuses the standard builder, keeps every non-transport tool
//! (memory / facts / scratchpad / TodoWrite / RunBackground / web tools /
//! LoadSkill / aliases / extra+session roots) intact.
//!
//! Container creation failure (image missing, daemon unreachable) is a hard
//! error — the provider never silently degrades to local execution. The
//! [`ToolSetProvider`] trait has no `Result` exit, so failure aborts the
//! process with a clear message (the CLI mirrors this contract for
//! non-`cloud-runtime` builds).
//!
//! Gated behind the `cloud-runtime` feature flag.

use std::path::PathBuf;
use std::sync::Arc;

use crate::tool_set_provider::{SandboxMode, ToolSetProvider};
use crate::tools::ToolRegistry;

use super::container_transport::ContainerTransport;

/// Error returned when the sandbox container cannot be created. The CLI
/// surfaces it as a fatal error — never a silent local fallback.
#[derive(Debug)]
pub struct ContainerSetupError(pub String);

impl std::fmt::Display for ContainerSetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "container sandbox creation failed: {} (refusing to fall back to local execution)",
            self.0
        )
    }
}

impl std::error::Error for ContainerSetupError {}

/// [`ToolSetProvider`] whose tools execute inside a container.
pub struct ContainerToolSetProvider {
    /// Host-side workspace directory (the bind source).
    pub workspace: PathBuf,
    pub shell_timeout_secs: u64,
    pub skills: Vec<crate::skills::Skill>,
}

impl ContainerToolSetProvider {
    pub fn new(
        workspace: PathBuf,
        shell_timeout_secs: u64,
        skills: Vec<crate::skills::Skill>,
    ) -> Self {
        Self {
            workspace,
            shell_timeout_secs,
            skills,
        }
    }

    /// Create the container transport. Public so callers that need a
    /// `Result` (tests, alternative wiring) can create it directly.
    pub async fn create_transport(
        workspace: &std::path::Path,
    ) -> Result<ContainerTransport, ContainerSetupError> {
        ContainerTransport::new(workspace)
            .await
            .map_err(|e| ContainerSetupError(format!("docker: {e}")))
    }
}

impl ContainerToolSetProvider {
    /// Result-returning registry build (issue #31 §C): container creation
    /// failure surfaces as `Err(ContainerSetupError)` so HTTP callers can
    /// map it to a per-session 503/500 instead of the process dying.
    /// Building the tool set itself never fails — only transport creation.
    pub async fn build_registry_result(&self) -> Result<ToolRegistry, ContainerSetupError> {
        let transport = Self::create_transport(&self.workspace).await?;
        // Issue §5: capabilities() must reflect the environment — probe
        // the toolchain once so the report is not permanently empty.
        transport.prime_toolchain().await;
        let shared: Arc<dyn crate::tools::ToolTransport> = Arc::new(transport);
        Ok(crate::tools::build_standard_tools_with_transport_opt(
            shared,
            &self.workspace,
            &[],
            None,
            &self.skills,
            self.shell_timeout_secs,
            None,
            None,
            None,
            None,
            false,
        ))
    }
}

impl ToolSetProvider for ContainerToolSetProvider {
    fn build_registry(&self) -> ToolRegistry {
        // `build_registry` is sync but container creation is async; same
        // pattern as `docker_provider.rs` (requires a multi-thread
        // runtime — the CLI already runs one). Creation failure aborts
        // with a clear message: silently degrading to host execution
        // would defeat the sandbox. Non-HTTP callers keep this fatal
        // contract; the HTTP layer uses `build_registry_result`.
        let workspace = self.workspace.clone();
        let transport = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(async move { Self::create_transport(&workspace).await })
        })
        .unwrap_or_else(|e| {
            eprintln!("recursive: {e}");
            std::process::exit(2);
        });
        // Issue §5: capabilities() must reflect the environment — probe
        // the toolchain once so the report is not permanently empty.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(transport.prime_toolchain())
        });
        let shared: Arc<dyn crate::tools::ToolTransport> = Arc::new(transport);

        // Plan B (issue §1): reuse the full standard builder so memory /
        // facts / scratchpad / TodoWrite / RunBackground / WatchFile /
        // EstimateTokens / web tools / LoadSkill / aliases / extra+session
        // roots all survive the container rebind — only the shared
        // transport differs from the local default.
        crate::tools::build_standard_tools_with_transport_opt(
            shared,
            &self.workspace,
            &[],
            None,
            &self.skills,
            self.shell_timeout_secs,
            None,
            None,
            None,
            None,
            // Issue #31 §3: run_background now executes through the shared
            // ContainerTransport (dies with the environment), so the
            // host-exec exclusion is lifted. watch_file / stop_loop remain
            // host-read-only utilities (no command execution); they read the
            // same bind-mounted workspace that the container sees.
            false,
        )
    }

    fn sandbox_mode(&self) -> SandboxMode {
        SandboxMode::Container
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_provider_sandbox_mode_is_container() {
        let p = ContainerToolSetProvider::new(PathBuf::from("/tmp"), 30, vec![]);
        assert_eq!(p.sandbox_mode(), SandboxMode::Container);
    }

    #[test]
    fn container_setup_error_message_mentions_no_fallback() {
        let e = ContainerSetupError("no daemon".into());
        let msg = format!("{e}");
        assert!(msg.contains("no daemon"));
        assert!(msg.contains("refusing to fall back"));
    }

    // ── Issue #245 batch 43 (B1 seat): registry-body pins ─────────────────
    //
    // These drive the real bollard client (DOCKER_HOST redirected under the
    // crate env lock) against a fake Docker API on a unique unix-domain
    // socket — a socket, because bollard 0.18's connect_with_unix_defaults
    // only honours a `unix://` DOCKER_HOST (an http:// value is filtered out
    // and falls back to /var/run/docker.sock). POST /containers/create
    // answers 201 {"Id": …}, everything else 204. The toolchain probe's exec
    // round-trips therefore fail and are swallowed (`probe_toolchain` →
    // empty vec), while create+start succeed — enough for the registry
    // build. The whole file compiles only under `cloud-runtime`; these
    // tests (and the mutants they kill) are invisible to the default
    // `cargo test --workspace` gate.

    /// Fake Docker API on a unique unix socket: create → 201 with an `Id`,
    /// everything else → 204. Returns the `unix://` URL for DOCKER_HOST, the
    /// socket path (best-effort removed by the caller) and the request log.
    #[cfg(unix)]
    fn spawn_fake_docker_api() -> (
        String,
        std::path::PathBuf,
        Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        use std::io::Read as _;
        use std::io::Write as _;
        let sock_path = std::env::temp_dir().join(format!(
            "recursive-itest-docker-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let listener =
            std::os::unix::net::UnixListener::bind(&sock_path).expect("bind fake docker socket");
        let addr = format!("unix://{}", sock_path.display());
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let reqs = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut sock) = stream else { continue };
                let reqs = reqs.clone();
                std::thread::spawn(move || {
                    let mut buf = vec![0u8; 8192];
                    let mut read = 0;
                    loop {
                        let n = sock.read(&mut buf[read..]).unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        read += n;
                        if buf[..read].windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let text = String::from_utf8_lossy(&buf[..read]).to_string();
                    let first = text.lines().next().unwrap_or("").to_string();
                    let is_create = first.contains("POST /containers/create");
                    reqs.lock().unwrap_or_else(|e| e.into_inner()).push(first);
                    let (status, body): (&str, &[u8]) = if is_create {
                        (
                            "201 Created",
                            br#"{"Id":"recursive-itest-fake","Warnings":[]}"#,
                        )
                    } else {
                        ("204 No Content", b"")
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes());
                    let _ = sock.write_all(body);
                });
            }
        });
        (addr, sock_path, requests)
    }

    /// Kill `replace ContainerToolSetProvider::build_registry_result -> …
    /// with Ok(Default::default())` (:87): the real body creates the
    /// container via the fake API (create recorded) and returns the standard
    /// tool set; the mutant returns an empty registry without dialling.
    #[cfg(unix)]
    #[allow(clippy::await_holding_lock)] // std env lock is fine: only same-crate tests contend
    #[tokio::test]
    async fn container_provider_build_registry_result_builds_the_standard_tool_set() {
        let _lock = crate::test_util::env_lock();
        let (addr, sock_path, log) = spawn_fake_docker_api();
        let saved = std::env::var_os("DOCKER_HOST");
        // SAFETY: process-global env mutation, serialised by `env_lock`.
        unsafe { std::env::set_var("DOCKER_HOST", &addr) };
        let p = ContainerToolSetProvider::new(PathBuf::from("/tmp"), 30, vec![]);
        let reg = p
            .build_registry_result()
            .await
            .expect("container tier builds against the fake Docker API");
        // SAFETY: restore before releasing the env lock.
        unsafe {
            match saved {
                Some(v) => std::env::set_var("DOCKER_HOST", v),
                None => std::env::remove_var("DOCKER_HOST"),
            }
        }
        let _ = std::fs::remove_file(&sock_path);
        assert!(
            reg.find_by_name("Bash").is_some(),
            "build_registry_result must wire the standard tool set (Bash missing)"
        );
        assert!(
            log.lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|r| r.contains("POST /containers/create")),
            "the real body must create the container"
        );
    }

    /// Kill `replace <impl ToolSetProvider for ContainerToolSetProvider>::build_registry
    /// -> ToolRegistry with Default::default()` (:116): same contract via the
    /// sync wrapper (block_in_place needs the multi-thread runtime).
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn container_provider_build_registry_builds_the_standard_tool_set() {
        let _lock = crate::test_util::env_lock();
        let (addr, sock_path, log) = spawn_fake_docker_api();
        let saved = std::env::var_os("DOCKER_HOST");
        // SAFETY: process-global env mutation, serialised by `env_lock`.
        unsafe { std::env::set_var("DOCKER_HOST", &addr) };
        let p = ContainerToolSetProvider::new(PathBuf::from("/tmp"), 30, vec![]);
        let reg = p.build_registry();
        // SAFETY: restore before releasing the env lock.
        unsafe {
            match saved {
                Some(v) => std::env::set_var("DOCKER_HOST", v),
                None => std::env::remove_var("DOCKER_HOST"),
            }
        }
        let _ = std::fs::remove_file(&sock_path);
        assert!(
            reg.find_by_name("Bash").is_some(),
            "build_registry must wire the standard tool set (Bash missing)"
        );
        assert!(
            log.lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|r| r.contains("POST /containers/create")),
            "the real body must create the container"
        );
    }
}
