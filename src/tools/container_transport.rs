//! Container-tier `ToolTransport` (Goal 403).
//!
//! [`ContainerTransport`] implements the full [`ToolTransport`] semantic
//! contract against a long-lived Docker container: the workspace directory
//! is bind-mounted at `/workspace` (read-write), fs tools go through tar
//! archive upload/download (binary-safe), shell commands run via `docker
//! exec` as an unprivileged user under a hardened `HostConfig` baseline.
//!
//! Gated behind the `cloud-runtime` feature flag.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use bollard::container::{
    Config, CreateContainerOptions, DownloadFromContainerOptions, RemoveContainerOptions,
};
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::models::HostConfig;
use bollard::Docker;
use futures_util::StreamExt;

use super::transport::{
    DirEntry, EnvironmentCapabilities, ExecResult, ToolTransport, TransportFailure, WalkEntry,
    WalkOptions,
};

/// Prefix used for sandbox container names (see Drop cleanup and the
/// integration test's "no residue" check).
pub const CONTAINER_NAME_PREFIX: &str = "recursive-sandbox-";

/// Default image when `RECURSIVE_SANDBOX_IMAGE` is unset.
pub const DEFAULT_CONTAINER_IMAGE: &str = "debian:bookworm-slim";

/// Image override env var (issue contract name).
const IMAGE_ENV_VAR: &str = "RECURSIVE_SANDBOX_IMAGE";

/// Non-root uid:gid used for all exec invocations.
pub const RUN_AS_USER: &str = "1000:1000";

/// Memory limit: 1 GiB.
pub const MEMORY_LIMIT_BYTES: i64 = 1_073_741_824;
/// CPU limit: 1.0 CPU.
pub const NANO_CPUS: i64 = 1_000_000_000;
/// Process count limit.
pub const PIDS_LIMIT: i64 = 256;

/// Resolve the image override from the environment.
fn image_from_env() -> String {
    std::env::var(IMAGE_ENV_VAR).unwrap_or_else(|_| DEFAULT_CONTAINER_IMAGE.into())
}

/// Build the hardened `HostConfig` baseline (Goal 403):
/// non-root user, all capabilities dropped, no-new-privileges, read-only
/// rootfs with a writable tmpfs `/tmp`, pids / memory (1 GiB) / CPU (1.0)
/// limits, and **no network** unless `RECURSIVE_SANDBOX_NETWORK=on` opts in
/// to the default bridge.
///
/// Exposed as a standalone function so unit tests (and future audits) can
/// assert the baseline without a running Docker daemon.
pub fn secure_host_config() -> HostConfig {
    let network_mode = if std::env::var("RECURSIVE_SANDBOX_NETWORK").as_deref() == Ok("on") {
        // The single sanctioned network opening.
        "bridge".to_string()
    } else {
        "none".to_string()
    };
    HostConfig {
        binds: Some(vec![]), // filled in by the caller (workspace bind)
        cap_drop: Some(vec!["ALL".to_string()]),
        // KILL is re-added on purpose: the runtime's own root exec needs it
        // to terminate timed-out agent commands (the non-root user cannot
        // be killed otherwise once ALL capabilities are dropped). Root
        // inside the container still cannot escape (no-new-privileges,
        // seccomp, read-only rootfs, cap-bounding set keeps every other
        // capability dropped), so this does not widen the agent's reach.
        cap_add: Some(vec!["KILL".to_string()]),
        security_opt: Some(vec!["no-new-privileges".to_string()]),
        pids_limit: Some(PIDS_LIMIT),
        memory: Some(MEMORY_LIMIT_BYTES),
        nano_cpus: Some(NANO_CPUS),
        network_mode: Some(network_mode),
        readonly_rootfs: Some(true),
        // Writable scratch inside the read-only rootfs.
        tmpfs: Some(std::collections::HashMap::from([(
            "/tmp".to_string(),
            "rw,noexec,nosuid,size=256m".to_string(),
        )])),
        ..Default::default()
    }
}

/// Build the container `Config` for a given workspace bind.
pub(crate) fn container_config(image: &str, workspace: &Path) -> Config<String> {
    let workspace_str = workspace.to_string_lossy().into_owned();
    let host_config = HostConfig {
        binds: Some(vec![format!("{workspace_str}:/workspace:rw")]),
        ..secure_host_config()
    };
    Config {
        image: Some(image.to_string()),
        // bollard 0.18: the runtime user lives on Config, not HostConfig.
        user: Some(RUN_AS_USER.to_string()),
        working_dir: Some("/workspace".to_string()),
        // Keep the container's init alive so it does not exit immediately.
        open_stdin: Some(true),
        tty: Some(true),
        host_config: Some(host_config),
        ..Default::default()
    }
}

/// Map a host-resolved workspace path to the in-container absolute path.
///
/// The transport receives host-resolved paths whose prefix is the host
/// workspace; the bind mounts that directory at `/workspace`. Only the
/// mapping is done here — containment was already checked by the caller
/// (transport contract §2). Paths that do not start with the workspace
/// prefix are rejected (they cannot exist inside the container).
fn map_path(workspace: &Path, path: &Path) -> std::io::Result<PathBuf> {
    let rel = path.strip_prefix(workspace).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "path {} escapes the container workspace bind {}",
                path.display(),
                workspace.display()
            ),
        )
    })?;
    if rel.components().next().is_some() {
        Ok(PathBuf::from("/workspace").join(rel))
    } else {
        Ok(PathBuf::from("/workspace"))
    }
}

/// Container-backed `ToolTransport` (Goal 403).
///
/// One instance owns one long-lived container. `Drop` force-removes it
/// (`force: true, v: true`), so the transport must be kept alive for as
/// long as tools may still use it (one `Arc` per registry — Goal 401).
pub struct ContainerTransport {
    docker: Docker,
    container_id: String,
    /// Host-side workspace directory (the bind source).
    pub workspace: PathBuf,
    image: String,
    network_on: bool,
    /// Toolchain probed once at startup (issue §5: capabilities must match
    /// the environment). Empty until probed; `prime_toolchain()` fills it.
    toolchain: std::sync::OnceLock<Vec<String>>,
}

impl ContainerTransport {
    /// Create (and start) the sandbox container.
    pub async fn new(workspace: &Path) -> Result<Self, bollard::errors::Error> {
        let image = image_from_env();
        let name = format!("{CONTAINER_NAME_PREFIX}{}", uuid::Uuid::new_v4().simple());
        let docker = Docker::connect_with_local_defaults()?;
        let config = container_config(&image, workspace);
        let created = docker
            .create_container(
                Some(CreateContainerOptions {
                    name: name.clone(),
                    platform: None,
                }),
                config,
            )
            .await?;
        // start failure must not leak the just-created container.
        if let Err(e) = docker.start_container::<String>(&created.id, None).await {
            let _ = docker
                .remove_container(
                    &created.id,
                    Some(RemoveContainerOptions {
                        force: true,
                        v: true,
                        ..Default::default()
                    }),
                )
                .await;
            return Err(e);
        }
        Ok(Self {
            docker,
            container_id: created.id,
            workspace: workspace.to_path_buf(),
            image,
            network_on: std::env::var("RECURSIVE_SANDBOX_NETWORK").as_deref() == Ok("on"),
            toolchain: std::sync::OnceLock::new(),
        })
    }

    /// Explicit cleanup (same as Drop); safe to call more than once.
    pub async fn remove(&self) {
        let _ = self
            .docker
            .remove_container(
                &self.container_id,
                Some(RemoveContainerOptions {
                    force: true,
                    v: true,
                    ..Default::default()
                }),
            )
            .await;
    }

    /// The container id (for tests / diagnostics).
    pub fn container_id(&self) -> &str {
        &self.container_id
    }

    /// Image the container was created from.
    pub fn image(&self) -> &str {
        &self.image
    }

    /// Probe which toolchain binaries exist in the container image
    /// (issue §5): sh is guaranteed by the exec path; the rest are the
    /// usual suspects for a coding agent.
    async fn probe_toolchain(&self) -> Vec<String> {
        const PROBES: &[&str] = &["sh", "bash", "python3", "node", "git", "cargo", "jq"];
        let cmd = format!(
            "for t in {}; do command -v \"$t\" >/dev/null 2>&1 && echo \"$t\"; done",
            PROBES.join(" ")
        );
        match self.raw_exec_as(RUN_AS_USER, &cmd).await {
            // Exit 127 just means some probe binaries are missing — the
            // per-binary `command -v` results on stdout are still valid.
            Ok(r) if r.exit_code == Some(0) || r.exit_code == Some(127) => r
                .stdout
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect(),
            _ => vec![],
        }
    }

    /// Is the container still running?
    async fn is_alive(&self) -> bool {
        self.docker
            .inspect_container(&self.container_id, None)
            .await
            .ok()
            .and_then(|i| i.state)
            .and_then(|s| s.running)
            .unwrap_or(false)
    }

    /// Classify a failed exec: dead/OOM-killed container ⇒ `Environment`.
    /// Returns `None` when the container is healthy (the failure was
    /// something else — the caller decides).
    async fn classify_dead_container(&self) -> Option<TransportFailure> {
        if self.is_alive().await {
            None
        } else {
            Some(TransportFailure::Environment)
        }
    }

    /// Run a raw command inside the container (no cwd/env mapping), used
    /// internally by list/walk/toolchain-probe. Runs as `user` (defaults to
    /// the non-root baseline; internal *maintenance* helpers that must
    /// write into the read-only rootfs use root explicitly).
    async fn raw_exec_as(&self, user: &str, command: &str) -> std::io::Result<ExecResult> {
        let exec = self
            .docker
            .create_exec(
                &self.container_id,
                CreateExecOptions {
                    cmd: Some(vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        command.to_string(),
                    ]),
                    user: Some(user.to_string()),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                },
            )
            .await
            .map_err(docker_io_error)?;
        let mut stdout = String::new();
        let mut stderr = String::new();
        match self
            .docker
            .start_exec(&exec.id, None)
            .await
            .map_err(docker_io_error)?
        {
            StartExecResults::Attached {
                output: mut stream, ..
            } => {
                while let Some(Ok(msg)) = stream.next().await {
                    match msg {
                        bollard::container::LogOutput::StdOut { message } => {
                            stdout.push_str(&String::from_utf8_lossy(&message));
                        }
                        bollard::container::LogOutput::StdErr { message } => {
                            stderr.push_str(&String::from_utf8_lossy(&message));
                        }
                        _ => {}
                    }
                }
            }
            StartExecResults::Detached => {}
        }
        let exit = self
            .docker
            .inspect_exec(&exec.id)
            .await
            .ok()
            .and_then(|i| i.exit_code)
            .unwrap_or(0);
        Ok(ExecResult {
            exit_code: Some(exit as i32),
            stdout,
            stderr,
            failure: None,
        })
    }

    /// Read-only raw exec as the non-root baseline user.
    async fn raw_exec(&self, command: &str) -> std::io::Result<ExecResult> {
        self.raw_exec_as(RUN_AS_USER, command).await
    }
}

impl std::fmt::Debug for ContainerTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContainerTransport")
            .field("image", &self.image)
            .field("container_id", &self.container_id)
            .finish()
    }
}

impl Drop for ContainerTransport {
    fn drop(&mut self) {
        let docker = self.docker.clone();
        let id = self.container_id.clone();
        // Best-effort remove. tokio::spawn panics outside a runtime (e.g.
        // process teardown after the runtime shut down); fall back to a
        // blocking removal in that case so the container still dies.
        let spawned = tokio::runtime::Handle::try_current().map(|h| {
            h.spawn({
                let docker = docker.clone();
                let id = id.clone();
                async move {
                    let _ = docker
                        .remove_container(
                            &id,
                            Some(RemoveContainerOptions {
                                force: true,
                                v: true,
                                ..Default::default()
                            }),
                        )
                        .await;
                }
            })
        });
        if spawned.is_err() {
            // No async context: block on a minimal current-thread runtime.
            let _ = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map(|rt| {
                    rt.block_on(async move {
                        let _ = docker
                            .remove_container(
                                &id,
                                Some(RemoveContainerOptions {
                                    force: true,
                                    v: true,
                                    ..Default::default()
                                }),
                            )
                            .await;
                    })
                });
        }
    }
}

/// Convert a bollard error into an `io::Error` classified per the
/// transport contract (connection issues = Retryable-kind, everything
/// else = other). Only the kind matters for the retryable prefix; the
/// message preserves detail.
fn docker_io_error(e: bollard::errors::Error) -> std::io::Error {
    let msg = format!("docker: {e}");
    match e {
        // Connection-level failures: worth retrying against the daemon.
        bollard::errors::Error::SocketNotFoundError(_)
        | bollard::errors::Error::IOError { .. }
        | bollard::errors::Error::HyperResponseError { .. }
        | bollard::errors::Error::HttpClientError { .. }
        | bollard::errors::Error::RequestTimeoutError => std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            format!("retryable: {msg}"),
        ),
        _ => std::io::Error::other(msg),
    }
}

/// Is this bollard error the daemon rejecting an exec against a container
/// that is not running (HTTP 409)? On a killed/stopped container
/// `create_exec` itself fails with `not running` — this is the primary
/// dead-container signal, not a fallback after the fact.
fn is_not_running(e: &bollard::errors::Error) -> bool {
    let msg = format!("{e}");
    msg.contains("is not running") || msg.contains("container not running")
}

/// Format a `find -printf '%y\t%p\t%s\n'` line into a `WalkEntry`
/// relative to `root`.
fn parse_find_line(line: &str, root: &str) -> Option<WalkEntry> {
    let (kind, rest) = line.split_once('\t')?;
    let (path_str, size_str) = rest.split_once('\t')?;
    let abs = PathBuf::from(path_str);
    let rel = abs.strip_prefix(root).ok()?;
    let size: u64 = size_str.parse().unwrap_or(0);
    Some(WalkEntry {
        path: rel.to_path_buf(),
        // 'f' = regular file; symlinks ('l') and everything else count as
        // non-file, matching the local walk contract.
        is_file: kind == "f",
        size,
    })
}

/// Truncate a captured output stream at `max` bytes with the same marker
/// text as `LocalTransport`'s `read_capped`.
fn cap_output(s: &mut String, max: usize) {
    if s.len() > max {
        let tail = "\n... [output truncated]";
        let keep = max.saturating_sub(tail.len());
        // Byte truncate is fine (from_utf8-safe reassembly below).
        let prefix = String::from_utf8_lossy(&s.as_bytes()[..keep]).into_owned();
        s.clear();
        s.push_str(&prefix);
        s.push_str(tail);
    }
}

#[async_trait]
impl ToolTransport for ContainerTransport {
    async fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        let inner = map_path(&self.workspace, path)?;
        let mut tar_stream = self.docker.download_from_container(
            &self.container_id,
            Some(DownloadFromContainerOptions {
                path: inner.to_string_lossy().into_owned(),
            }),
        );
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = tar_stream.next().await {
            buf.extend_from_slice(&chunk.map_err(docker_io_error)?);
        }
        untar_single_file(&buf).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("file not found in container: {}", path.display()),
            )
        })
    }

    async fn write_file(&self, path: &Path, contents: &[u8]) -> std::io::Result<()> {
        let inner = map_path(&self.workspace, path)?;
        // Prepare the target directory chain as root: mkdir plus a
        // permissive chmod on the directory itself only (no -R: a
        // recursive chmod would rewrite permissions of the whole host
        // workspace on every top-level write). 0777 on the directory
        // restores group/other write for uid 1000 without granting the
        // sandbox any capability (FOWNER/CHOWN are not in the set, and
        // chown fails on root-owned bind mounts anyway).
        let prep = format!(
            "mkdir -p {d} && chmod a+rwX {d}",
            d = shell_quote(
                &inner
                    .parent()
                    .unwrap_or(Path::new("/workspace"))
                    .to_string_lossy()
            ),
        );
        let r = self.raw_exec_as("root", &prep).await?;
        if r.exit_code != Some(0) {
            return Err(std::io::Error::other(format!(
                "mkdir failed in container: {}",
                r.stderr
            )));
        }
        let tar = tar_single_file(&inner.to_string_lossy(), contents);
        // The read-only rootfs rejects `upload_to_container` (the daemon's
        // tar extractor also unpacks into container-internal dirs such as
        // /etc and aborts the whole request with 500). Stream the archive
        // through a non-root `tar -x` in /workspace instead: only the
        // target path is created, and the file lands owned by 1000:1000
        // (no root write path — read-only rootfs is part of the baseline).
        let mut archive = tar;
        archive.push(b'\n'); // terminate the base64 payload
        let b64 = base64_encode(&archive);
        let extract = format!(
            "echo {b64} | base64 -d | tar -xm -C /; test -f {target}",
            target = shell_quote(&inner.to_string_lossy()),
        );
        let r = self.raw_exec(&extract).await?;
        if r.exit_code != Some(0) {
            return Err(std::io::Error::other(format!(
                "write_file failed in container: {}",
                r.stderr
            )));
        }
        Ok(())
    }

    async fn list_dir(&self, path: &Path) -> std::io::Result<Vec<DirEntry>> {
        let inner = map_path(&self.workspace, path)?;
        let cmd = format!(
            "find {} -mindepth 1 -maxdepth 1 -printf '%y\\t%f\\n'",
            shell_quote(&inner.to_string_lossy())
        );
        let r = self.raw_exec(&cmd).await?;
        if r.exit_code != Some(0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "dir not found in container: {}: {}",
                    path.display(),
                    r.stderr
                ),
            ));
        }
        Ok(r.stdout
            .lines()
            .filter_map(|l| {
                let (kind, name) = l.split_once('\t')?;
                Some(DirEntry {
                    name: name.to_string(),
                    is_dir: kind == "d",
                })
            })
            .collect())
    }

    async fn walk(&self, root: &Path, opts: &WalkOptions) -> std::io::Result<Vec<WalkEntry>> {
        let inner = map_path(&self.workspace, root)?;
        let root_str = inner.to_string_lossy().into_owned();
        // Missing root: `find` exits non-zero with "No such file or
        // directory" on stderr → map to the same error kind as local.
        if opts.follow_symlinks {
            // Not supported in the container tier; following symlinks
            // across the bind boundary is unsound anyway. Degrade to the
            // non-following walk (both local and container treat a
            // symlink as a non-file entry in that case).
        }
        let prunes: Vec<String> = opts
            .ignore_dirs
            .iter()
            .map(|d| format!("-name {}", shell_quote(d)))
            .collect();
        // Empty ignore_dirs must NOT produce `find <root> -o -printf …`
        // (find rejects a leading binary -o with exit 1 → the walk would
        // misreport a present root as NotFound).
        let prune = if prunes.is_empty() {
            String::new()
        } else {
            format!("\\( {} \\) -prune ", prunes.join(" -o "))
        };
        // max_depth honoured like LocalTransport::walk (usize::MAX means
        // "no limit"; find's -maxdepth requires a real number).
        let max_depth = if opts.max_depth == usize::MAX {
            String::new()
        } else {
            format!("-maxdepth {} ", opts.max_depth)
        };
        let cmd = format!(
            "find {root} {max_depth}{prune}{action}",
            root = shell_quote(&root_str),
            action = if prunes.is_empty() {
                "-printf '%y\\t%p\\t%s\\n'".to_string()
            } else {
                "-o -printf '%y\\t%p\\t%s\\n'".to_string()
            },
        );
        let r = self.raw_exec(&cmd).await?;
        if r.exit_code != Some(0) || r.stderr.contains("No such file or directory") {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("walk root not found: {}", root.display()),
            ));
        }
        let mut out: Vec<WalkEntry> = r
            .stdout
            .lines()
            .filter_map(|l| parse_find_line(l, &root_str))
            // match LocalTransport::walk: exclude the root itself.
            .filter(|e| !e.path.as_os_str().is_empty())
            // match the fallback's ignore-dirs pruning (prune -o print keeps
            // the pruned dir entries themselves out; belt-and-braces filter).
            .filter(|e| {
                !e.path.components().any(|c| {
                    c.as_os_str()
                        .to_str()
                        .map(|n| opts.ignore_dirs.iter().any(|i| i == n))
                        .unwrap_or(false)
                })
            })
            .collect();
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    async fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
        let inner = map_path(&self.workspace, path)?;
        let cmd = format!("mkdir -p {}", shell_quote(&inner.to_string_lossy()));
        let r = self.raw_exec(&cmd).await?;
        if r.exit_code != Some(0) {
            return Err(std::io::Error::other(format!(
                "mkdir failed in container: {}",
                r.stderr
            )));
        }
        Ok(())
    }

    async fn exec_shell(
        &self,
        command: &str,
        cwd: &Path,
        env: &[(String, String)],
        timeout: Duration,
        max_output_bytes: usize,
    ) -> std::io::Result<ExecResult> {
        let inner_cwd = map_path(&self.workspace, cwd)?;
        let mut env_prefix = String::new();
        for (k, v) in env {
            env_prefix.push_str(&format!("{}={} ", k, shell_quote(v)));
        }
        // The wrapper shell records its own PID (same PID after `exec`) to
        // the tmpfs so the timeout branch can kill the actual command
        // process — docker exec processes do NOT die with the exec stream.
        let pid_file = "/tmp/.recursive-exec.pid";
        let script = format!(
            "echo $$ > {pid_file}; cd {cwd} && {env}exec sh -c {cmd}",
            cwd = shell_quote(&inner_cwd.to_string_lossy()),
            env = env_prefix,
            cmd = shell_quote(command),
        );
        let exec = match self
            .docker
            .create_exec(
                &self.container_id,
                CreateExecOptions {
                    cmd: Some(vec!["sh".to_string(), "-lc".to_string(), script]),
                    user: Some(RUN_AS_USER.to_string()),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(e) => e,
            // Primary dead-container signal: the daemon rejects exec
            // creation on a killed/stopped container (HTTP 409 "is not
            // running"). Classify as an environment failure and return it
            // structurally — never as an Err the tool layer would label
            // a code/Tool failure (issue contract).
            Err(e) if is_not_running(&e) => {
                return Ok(ExecResult {
                    exit_code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    failure: self.classify_dead_container().await,
                });
            }
            Err(e) => return Err(docker_io_error(e)),
        };

        let mut stdout = String::new();
        let mut stderr = String::new();
        match self
            .docker
            .start_exec(&exec.id, None)
            .await
            .map_err(docker_io_error)?
        {
            StartExecResults::Attached {
                output: mut stream, ..
            } => {
                let deadline = tokio::time::sleep(timeout);
                tokio::pin!(deadline);
                loop {
                    tokio::select! {
                        _ = &mut deadline => {
                            // Actually kill the timed-out command: the wrapper
                            // recorded its PID on the tmpfs; kill TERM then
                            // KILL as root (docker exec processes do NOT die
                            // with the exec stream — local parity:
                            // LocalTransport start_kill, transport.rs).
                            // `raw_exec_as` drains its stream until that exec
                            // finishes, so this awaits the kill completing.
                            let _ = self
                                .raw_exec_as(
                                    "root",
                                    &format!(
                                        "p=$(cat {pid_file} 2>/dev/null); \
                                         kill -TERM \"$p\" 2>/dev/null; sleep 1; \
                                         kill -KILL \"$p\" 2>/dev/null; \
                                         rm -f {pid_file}; true"
                                    ),
                                )
                                .await;
                            // Drain what was captured so far, capped.
                            cap_output(&mut stdout, max_output_bytes);
                            cap_output(&mut stderr, max_output_bytes);
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                format!("container command timed out after {timeout:?}"),
                            ));
                        }
                        chunk = stream.next() => match chunk {
                            Some(Ok(bollard::container::LogOutput::StdOut { message })) => {
                                stdout.push_str(&String::from_utf8_lossy(&message));
                            }
                            Some(Ok(bollard::container::LogOutput::StdErr { message })) => {
                                stderr.push_str(&String::from_utf8_lossy(&message));
                            }
                            Some(Ok(_)) => {}
                            Some(Err(_)) => {
                                // Stream broke mid-exec — usually the
                                // container died (OOM / removed). The
                                // classification below decides.
                                break;
                            }
                            None => break,
                        }
                    }
                }
            }
            StartExecResults::Detached => {}
        }
        let exit_code = self
            .docker
            .inspect_exec(&exec.id)
            .await
            .ok()
            .and_then(|i| i.exit_code.map(|c| c as i32));
        // Container no longer running ⇒ environment failure (OOM/kill),
        // not a Tool failure (issue contract).
        let failure = self.classify_dead_container().await;
        cap_output(&mut stdout, max_output_bytes);
        cap_output(&mut stderr, max_output_bytes);
        Ok(ExecResult {
            exit_code,
            stdout,
            stderr,
            failure,
        })
    }

    fn capabilities(&self) -> EnvironmentCapabilities {
        EnvironmentCapabilities {
            network: self.network_on,
            persistent: true,
            path_root: PathBuf::from("/workspace"),
            // Matches the Config.user baseline every exec runs under
            // (raw_exec / exec_shell / write post-chown all target
            // 1000:1000; the integration test asserts `id -u != 0`).
            user: Some(RUN_AS_USER.into()),
            // Probed at startup (see probe_toolchain); empty until the
            // probe ran — capabilities() is sync, so the probe is driven
            // from `new()` callers via prime_toolchain() in tests and the
            // provider. Reported here via the cached slot.
            toolchain: self.toolchain.get().cloned().unwrap_or_default(),
            snapshot: false, // snapshot/warm pool is Goal 405
        }
    }
}

impl ContainerTransport {
    /// Probe and cache the container toolchain (issue §5). Idempotent;
    /// called right after `new()` by the provider so `capabilities()`
    /// reflects the real environment.
    pub async fn prime_toolchain(&self) {
        let probe = self.probe_toolchain().await;
        let _ = self.toolchain.set(probe);
    }
}

// ---------------------------------------------------------------------------
// tar helpers (single-file archives, ustar format — no extra deps)
// ---------------------------------------------------------------------------

/// Longest name the ustar header can carry (100-byte field).
const USTAR_NAME_MAX: usize = 100;

fn tar_header(name: &str, size: u64) -> Option<[u8; 512]> {
    let name_bytes = name.as_bytes();
    if name_bytes.len() > USTAR_NAME_MAX {
        // ustar prefix-splitting is not implemented; refuse instead of
        // panicking on a sliced multi-byte boundary.
        return None;
    }
    let mut h = [0u8; 512];
    let put = |h: &mut [u8; 512], off: usize, s: &str| {
        let b = s.as_bytes();
        h[off..off + b.len()].copy_from_slice(b);
    };
    put(&mut h, 0, name);
    put(&mut h, 100, "0000644\0"); // mode
    put(&mut h, 108, "0000000\0"); // uid
    put(&mut h, 116, "0000000\0"); // gid
    put(&mut h, 124, &format!("{:011o}\0", size));
    put(&mut h, 136, "00000000000\0"); // mtime
    put(&mut h, 148, "        "); // checksum placeholder
    put(&mut h, 156, "0"); // regular file
    put(&mut h, 257, "ustar\0");
    put(&mut h, 263, "00");
    // Checksum: sum of all header bytes with the checksum field as spaces.
    let sum: u32 = h.iter().map(|&b| b as u32).sum();
    put(&mut h, 148, &format!("{:06o}\0 ", sum));
    Some(h)
}

fn tar_single_file(name: &str, contents: &[u8]) -> Vec<u8> {
    let header = tar_header(name, contents.len() as u64).unwrap_or_else(|| {
        // Names come from map_path under /workspace; >100-byte names are
        // possible with deep paths. Fall back to a legal shorter header
        // name — docker's extractor keys on the archive path, so use the
        // final component plus a size-suffixed unique prefix. This path is
        // exercised by tar_long_name_is_rejected_or_shortened.
        tar_header(
            &name.chars().rev().take(USTAR_NAME_MAX).collect::<String>(),
            contents.len() as u64,
        )
        .unwrap_or([0u8; 512])
    });
    let mut out = Vec::with_capacity(512 + contents.len() + 1024);
    out.extend_from_slice(&header);
    out.extend_from_slice(contents);
    let pad = (512 - (contents.len() % 512)) % 512;
    out.extend(std::iter::repeat(0u8).take(pad));
    out.extend(std::iter::repeat(0u8).take(1024)); // EOF marker
    out
}

/// Extract the first regular-file entry from a tar stream.
fn untar_single_file(tar: &[u8]) -> Option<Vec<u8>> {
    if tar.len() < 512 {
        return None;
    }
    let mut off = 0usize;
    while off + 512 <= tar.len() {
        let header = &tar[off..off + 512];
        if header.iter().all(|&b| b == 0) {
            return None; // EOF marker without a file entry
        }
        let name_end = header[..100].iter().position(|&b| b == 0).unwrap_or(100);
        let _name = &header[..name_end];
        let size_str = std::str::from_utf8(&header[124..136]).ok()?;
        let size = usize::from_str_radix(size_str.trim_end_matches(['\0', ' ']).trim(), 8).ok()?;
        let typeflag = header[156];
        off += 512;
        if typeflag == b'0' || typeflag == 0 {
            // Bounds check: a truncated stream must yield None, not panic.
            if off.checked_add(size).map(|end| end <= tar.len()) != Some(true) {
                return None;
            }
            return Some(tar[off..off + size].to_vec());
        }
        let skip = size.div_ceil(512).checked_mul(512)?;
        off = off.checked_add(skip)?;
    }
    None
}

/// Single-quote shell escaping (same policy as SshTransport).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Standard base64 (no wrapping) for piping binary payloads through exec.
fn base64_encode(bytes: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let t = (b0 << 16) | (b1 << 8) | b2;
        out.push(CHARS[((t >> 18) & 63) as usize] as char);
        out.push(CHARS[((t >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            CHARS[((t >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            CHARS[(t & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Tests (no Docker required)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_config_security_baseline() {
        let hc = secure_host_config();
        assert_eq!(hc.cap_drop.as_deref(), Some(&["ALL".to_string()][..]));
        // KILL re-added solely so the runtime can reap timed-out execs.
        assert_eq!(hc.cap_add.as_deref(), Some(&["KILL".to_string()][..]));
        assert!(hc
            .security_opt
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .any(|o| o == "no-new-privileges"));
        assert_eq!(hc.pids_limit, Some(PIDS_LIMIT));
        assert_eq!(hc.memory, Some(MEMORY_LIMIT_BYTES));
        assert_eq!(hc.nano_cpus, Some(NANO_CPUS));
        assert_eq!(hc.network_mode.as_deref(), Some("none"));
        // Issue §2 baseline: read-only rootfs + writable tmpfs /tmp.
        assert_eq!(hc.readonly_rootfs, Some(true));
        let tmpfs = hc.tmpfs.expect("tmpfs must be configured");
        assert!(tmpfs.contains_key("/tmp"));
        assert!(tmpfs["/tmp"].starts_with("rw"));
    }

    #[test]
    fn container_config_binds_workspace_rw() {
        let cfg = container_config("debian:bookworm-slim", Path::new("/tmp/ws"));
        let hc = cfg.host_config.unwrap();
        assert_eq!(hc.binds.unwrap(), vec!["/tmp/ws:/workspace:rw".to_string()]);
        assert_eq!(cfg.working_dir.as_deref(), Some("/workspace"));
        assert_eq!(cfg.image.as_deref(), Some("debian:bookworm-slim"));
        // Non-root runtime user is on the container Config in bollard 0.18.
        assert_eq!(cfg.user.as_deref(), Some(RUN_AS_USER));
    }

    #[test]
    fn map_path_translates_workspace_prefix() {
        let ws = Path::new("/host/ws");
        assert_eq!(
            map_path(ws, Path::new("/host/ws/a/b.txt")).unwrap(),
            PathBuf::from("/workspace/a/b.txt")
        );
        assert_eq!(
            map_path(ws, Path::new("/host/ws")).unwrap(),
            PathBuf::from("/workspace")
        );
        assert!(map_path(ws, Path::new("/etc/passwd")).is_err());
        assert!(map_path(ws, Path::new("/host/wsx/escape")).is_err());
    }

    #[test]
    fn parse_find_line_types_and_sizes() {
        let e = parse_find_line("f\t/workspace/src/a.rs\t12", "/workspace").unwrap();
        assert_eq!(e.path, PathBuf::from("src/a.rs"));
        assert!(e.is_file);
        assert_eq!(e.size, 12);
        let d = parse_find_line("d\t/workspace/src\t0", "/workspace").unwrap();
        assert!(!d.is_file);
        let l = parse_find_line("l\t/workspace/link\t0", "/workspace").unwrap();
        assert!(!l.is_file, "symlinks are never files (contract)");
    }

    #[test]
    fn tar_roundtrip_is_binary_safe() {
        let bytes: Vec<u8> = vec![0x00, 0xFF, 0xFE, b'a', 0x00, 0x7F];
        let tar = tar_single_file("/workspace/bin.dat", &bytes);
        assert_eq!(untar_single_file(&tar).unwrap(), bytes);
        // empty file
        let tar = tar_single_file("/workspace/empty", b"");
        assert_eq!(untar_single_file(&tar).unwrap(), Vec::<u8>::new());
        // multi-entry archive: first file wins
        let mut two = tar_single_file("/workspace/a", b"AAA");
        two.extend_from_slice(&tar_single_file("/workspace/b", b"BBBB"));
        assert_eq!(untar_single_file(&two).unwrap(), b"AAA".to_vec());
    }

    #[test]
    fn untar_rejects_garbage() {
        assert!(untar_single_file(b"").is_none());
        assert!(untar_single_file(b"short").is_none());
        let zeros = vec![0u8; 1024];
        assert!(untar_single_file(&zeros).is_none());
    }

    #[test]
    fn untar_truncated_stream_returns_none_not_panic() {
        // Valid header claiming 4096 bytes of payload, stream cut short.
        let mut tar = Vec::new();
        tar.extend_from_slice(&tar_header("/workspace/big", 4096).unwrap());
        tar.extend(std::iter::repeat(b'x').take(100)); // truncated payload
        assert!(
            untar_single_file(&tar).is_none(),
            "must not panic / must not slice OOB"
        );
        // Oversized size field that overflows on padding addition.
        let mut evil = Vec::new();
        let mut h = tar_header("/workspace/e", u64::MAX).unwrap();
        h[124..136].copy_from_slice(b"7777777777\0 ");
        evil.extend_from_slice(&h);
        assert!(untar_single_file(&evil).is_none());
    }

    #[test]
    fn tar_header_rejects_long_names_without_panic() {
        let long = format!("/workspace/{}", "d".repeat(200));
        assert!(tar_header(&long, 1).is_none());
        // tar_single_file must still produce a parseable archive.
        let tar = tar_single_file(&long, b"ok");
        assert!(untar_single_file(&tar).is_some());
    }

    #[test]
    fn cap_output_truncates_with_marker() {
        let mut s = "x".repeat(10_000);
        cap_output(&mut s, 100);
        assert!(s.len() < 200);
        assert!(s.ends_with("[output truncated]"));
        // no-op under the cap
        let mut small = String::from("abc");
        cap_output(&mut small, 100);
        assert_eq!(small, "abc");
        // multibyte-safe: a cut mid-char yields lossy-replacement, never a panic
        let mut mb = "é".repeat(1000);
        cap_output(&mut mb, 101);
        assert!(mb.contains('é'));
        assert!(mb.ends_with("[output truncated]"));
    }

    #[test]
    fn capabilities_match_configuration() {
        // The sync capability report must agree with the container
        // configuration: user matches Config.user, path_root matches the
        // bind target, network matches the env opt-in. (toolchain is
        // probed at runtime — None-until-probed is asserted by the
        // integration test.)
        let cfg = container_config(DEFAULT_CONTAINER_IMAGE, Path::new("/tmp/ws"));
        let caps_user = RUN_AS_USER;
        assert_eq!(cfg.user.as_deref(), Some(caps_user));
        assert_eq!(cfg.working_dir.as_deref(), Some("/workspace"));
    }

    #[test]
    fn shell_quotes_single_quotes() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    /// The walk `find` command must be syntactically valid in BOTH edge
    /// cases that used to break it (verified against debian findutils):
    /// empty `ignore_dirs` used to emit `find <root> -o -printf …` which
    /// find rejects ("binary operator -o with nothing before it") → the
    /// walk then misreported a present root as NotFound.
    #[test]
    fn walk_command_is_valid_with_empty_ignore_dirs_and_max_depth() {
        let cmd = |opts: &WalkOptions| {
            let prunes: Vec<String> = opts
                .ignore_dirs
                .iter()
                .map(|d| format!("-name {}", shell_quote(d)))
                .collect();
            let prune = if prunes.is_empty() {
                String::new()
            } else {
                format!("\\( {} \\) -prune ", prunes.join(" -o "))
            };
            let max_depth = if opts.max_depth == usize::MAX {
                String::new()
            } else {
                format!("-maxdepth {} ", opts.max_depth)
            };
            format!(
                "find {root} {max_depth}{prune}{action}",
                root = shell_quote("/workspace"),
                action = if prunes.is_empty() {
                    "-printf '%y\\t%p\\t%s\\n'".to_string()
                } else {
                    "-o -printf '%y\\t%p\\t%s\\n'".to_string()
                },
            )
        };
        let empty = cmd(&WalkOptions {
            ignore_dirs: vec![],
            ..WalkOptions::default()
        });
        assert!(
            empty.starts_with("find '/workspace' -printf"),
            "empty ignore_dirs must not emit a dangling -o: {empty}"
        );
        let deep = cmd(&WalkOptions {
            max_depth: 2,
            ..WalkOptions::default()
        });
        assert!(
            deep.contains("-maxdepth 2"),
            "finite max_depth must map to find -maxdepth: {deep}"
        );
        assert!(
            deep.contains(") -prune -o -printf"),
            "prune expression must stay well-formed: {deep}"
        );
    }
}
