//! Process-level hardening (issue #150).
//!
//! Issue #89 scrubbed credential-shaped variables out of *child* command
//! environments, but the service process kept its own execve-time
//! environment intact. On Linux that block is mirrored at
//! `/proc/<pid>/environ`, so a prompt-injected child — the default tier
//! spawns `/bin/sh` as a direct child, making the service process its
//! `$PPID` — could still walk off with every credential:
//!
//! ```sh
//! tr '\0' '\n' < /proc/$PPID/environ
//! ```
//!
//! `std::env::remove_var` cannot close this: libc's `unsetenv` only edits
//! the C `environ` pointer array, never the kernel's original environment
//! block. Three layers therefore work together:
//!
//! 1. [`harden_process`] — Linux: `prctl(PR_SET_DUMPABLE, 0)` makes the
//!    kernel expose `/proc/<pid>/environ` (and `mem`/`maps`) as root-owned,
//!    so a same-uid child's `open()` fails with `EACCES` and only
//!    `CAP_SYS_PTRACE` holders can still read it. This is the main fix.
//! 2. [`scrub_process_env`] — every platform: once `Config::from_env` has
//!    consumed this process's own credentials, drop them from the live
//!    `environ` so children spawned *without* issue #89's scrub (MCP stdio
//!    servers, `git` helpers, …), crash reports and `environ` dumps in logs
//!    no longer carry them. Defence in depth, not a substitute for (1).
//! 3. [`reexec_clean_env`] — unix without a kernel switch (macOS, …): the
//!    kernel's copy cannot be hidden, so the process re-executes itself with
//!    an environment that never held the credentials (issue #226; details
//!    under "Platform coverage").
//!
//! Call [`harden_process`] once, as early as possible — before any
//! execution surface (tool transports, `run_background`, the HTTP server)
//! can spawn a child. The CLI does this at the top of its `main()`;
//! library embedders must call it themselves after process start and
//! should [`HardenOutcome::report`] the result once logging is up.
//!
//! Trade-off: `PR_SET_DUMPABLE=0` also disables core dumps for the process.
//! That is intentional — a core dump would contain the same secrets — and
//! is not a regression.
//!
//! # Platform coverage (measured, not assumed)
//!
//! * **Linux** — closed by `PR_SET_DUMPABLE=0`
//!   (`tests/issue89_parent_environ.rs` reads `/proc/$PPID/environ` from a
//!   real `Bash` child and asserts the credentials are gone).
//! * **macOS / other non-Linux unix** — *closed by the #226 re-exec*. There
//!   is no `/proc`, and `sysctl(KERN_PROCARGS2)` (what `ps -Eww -p <pid>` /
//!   `ps eww` call) returns the execve-time environment of any same-uid
//!   process **unless the target is an Apple platform binary**. Measured on
//!   macOS 15.7: `/bin/sh` and `/bin/sleep` children show zero environment
//!   entries, a Homebrew `python3` or a freshly `cc`-built binary shows all
//!   of them — and ad-hoc `codesign -o runtime` (hardened runtime) does
//!   **not** change that. A cargo-built `recursive` is a non-platform
//!   binary, so the kernel's exec-time copy is the leak. No syscall can
//!   hide it after the fact, so the process **re-executes itself with a
//!   scrubbed environment** ([`reexec_clean_env`]): the exec-time block the
//!   new image carries is the whitelist the parent built, process-credential
//!   variables travel out-of-band over an inherited pipe fd
//!   ([`CredentialHandoff`], the ssh-agent/sudo pattern) and are restored
//!   into the live environment before config parsing. `execve` keeps the
//!   pid, so supervisors and `$PPID` accounting are unaffected.
//! * **Windows** — *open* (same-user `ReadProcessMemory` of the PEB); same
//!   WARN. `harden_process` compiles to a no-op there and nothing in this
//!   module touches spawn behaviour. Windows has no `execve` semantics, so
//!   [`reexec_clean_env`] is a documented no-op there (#207 convention).

/// Result of [`harden_process`]. Plain data so callers can log it *after*
/// their logging is initialised (the CLI hardens before it has a
/// subscriber) and so tests can assert the arm that ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardenOutcome {
    /// `prctl(PR_SET_DUMPABLE, 0)` succeeded: `/proc/<pid>/environ` is now
    /// unreadable to same-uid children.
    Applied,
    /// The syscall failed. The process keeps running (fail-open, see
    /// [`HardenOutcome::report`]) with issue #89's child-env scrub as the
    /// remaining layer; `errno` is the kernel's reason.
    Failed {
        /// Raw `errno` from the failed `prctl` call.
        errno: i32,
    },
    /// No kernel mechanism exists on this platform (macOS, Windows, …).
    Unsupported,
}

impl HardenOutcome {
    /// `true` when the exec-time environment copy is actually hidden.
    pub fn is_applied(&self) -> bool {
        matches!(self, HardenOutcome::Applied)
    }

    /// Emit the outcome through `tracing`. Call once logging is up.
    ///
    /// A failed `prctl` is WARN rather than fatal: `PR_SET_DUMPABLE` cannot
    /// legitimately fail for argument `0`, so a failure means a seccomp
    /// filter or an exotic kernel refused it — refusing to start would turn
    /// a confidentiality gap that still has issue #89's scrub behind it into
    /// an availability outage. The trace names the mechanism and `errno` so
    /// the deployment can be fixed rather than the gap silently accepted.
    pub fn report(&self) {
        match self {
            HardenOutcome::Applied => {
                tracing::debug!("hardening: prctl(PR_SET_DUMPABLE, 0) applied — /proc/<pid>/environ hidden from same-uid children");
            }
            HardenOutcome::Failed { errno } => {
                let reason = std::io::Error::from_raw_os_error(*errno);
                tracing::warn!(
                    mechanism = "prctl(PR_SET_DUMPABLE, 0)",
                    errno = *errno,
                    "hardening: prctl(PR_SET_DUMPABLE, 0) failed ({reason}); \
                     /proc/<pid>/environ stays readable to same-uid children. \
                     Continuing fail-open: issue #89's child-env scrub is still in \
                     force, and refusing to start would trade a confidentiality gap \
                     for an outage (issue #150)"
                );
            }
            HardenOutcome::Unsupported => {
                tracing::debug!(
                    os = std::env::consts::OS,
                    "hardening: no kernel mechanism hides this process's exec-time environment on this platform"
                );
            }
        }
    }

    /// WARN when credentials were present in this process's environment and
    /// the exec-time copy is still readable by same-uid processes
    /// ([`HardenOutcome::Failed`] or [`HardenOutcome::Unsupported`]).
    ///
    /// Quiet when the environment carried no credential-shaped variable:
    /// there is nothing to leak, so nothing is degraded.
    pub fn warn_if_exposed(&self, scrub: &ScrubReport) {
        if self.is_applied() {
            return;
        }
        let exposed = scrub.removed.len() + scrub.retained.len();
        if exposed == 0 {
            return;
        }
        let names: Vec<&str> = scrub
            .removed
            .iter()
            .chain(scrub.retained.iter())
            .map(String::as_str)
            .collect();
        tracing::warn!(
            os = std::env::consts::OS,
            count = exposed,
            variables = ?names,
            "hardening: credential variables were in this process's exec-time \
             environment and nothing hides that copy on this platform — a same-uid \
             process can still read it (macOS: `ps -Eww -p <pid>` / \
             sysctl KERN_PROCARGS2; Windows: ReadProcessMemory). The live environ \
             has been scrubbed, but prefer a secrets file or credential proxy over \
             env injection (issue #150)"
        );
    }
}

/// Make this process's environment (and memory) unreadable to same-uid
/// children on Linux.
///
/// Returns [`HardenOutcome::Unsupported`] off Linux — see the module docs
/// for what covers macOS instead ([`reexec_clean_env`], issue #226) and
/// what stays exposed on Windows.
///
/// Idempotent: calling it more than once is harmless and returns the same
/// outcome.
#[must_use = "a failed or unsupported hardening must be reported (owner ruling: every degradation is observable)"]
pub fn harden_process() -> HardenOutcome {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `prctl` is a plain syscall wrapper. `PR_SET_DUMPABLE`
        // takes one integer argument; passing zero is the documented
        // "not dumpable" setting and has no memory-safety preconditions.
        let rc = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
        if rc == 0 {
            HardenOutcome::Applied
        } else {
            HardenOutcome::Failed {
                errno: std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        HardenOutcome::Unsupported
    }
}

/// Environment prefix of the HTTP inbound-auth configuration
/// (`RECURSIVE_HTTP_AUTH_KEYS`, `RECURSIVE_HTTP_AUTH_JWT_SECRET`, …).
///
/// `recursive http` reads these only when it builds the router, which is
/// *after* the generic startup scrub, so that command passes this prefix as
/// `keep_prefixes` and scrubs them itself once the router exists.
pub const HTTP_AUTH_ENV_PREFIX: &str = "RECURSIVE_HTTP_AUTH_";

/// Names that match the credential pattern but are switches, not secrets,
/// and are re-read after startup — removing them would change behaviour.
///
/// `RECURSIVE_HTTP_AUTH_INSECURE_OK` is a boolean dev-only bypass that
/// `auth_middleware` consults on *every* request; dropping it after the
/// router is built would flip a debug server from "open" to 503 after the
/// first request.
const NON_CREDENTIAL_EXCEPTIONS: &[&str] = &["RECURSIVE_HTTP_AUTH_INSECURE_OK"];

/// Issue #150: is `name` one of *this process's own* credential variables —
/// the ones `Config::from_env` and the HTTP auth builder consume and that
/// have no business staying in the live environment afterwards?
///
/// Scope is deliberately the `RECURSIVE_` namespace only, matched on whole
/// `_`-delimited segments: `KEY`/`KEYS`/`SECRET`/`TOKEN`/`AUTH`. Segment
/// matching is what keeps `RECURSIVE_MAX_TOKENS` (a config knob that
/// `resolve_max_tokens` re-reads) out — `TOKENS` is not `TOKEN`.
///
/// Third-party names (`OPENAI_API_KEY`, `GITHUB_TOKEN`, …) are **not**
/// matched even though issue #89's *child* scrub drops them: the service
/// process itself hands them to consumers that read the environment by
/// name at arbitrary later points — `.mcp.json` `${VAR}` expansion at MCP
/// server spawn, the credential store's `env:` layer, `http_call`'s
/// env-named auth header — so removing them from the parent would break
/// those features, not harden anything. For the same reason this is a
/// different predicate from `transport::is_sensitive_env_var`, which treats
/// every `RECURSIVE_*` as sensitive because a *child* never needs any of
/// them.
pub fn is_process_credential_env(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    let Some(rest) = upper.strip_prefix("RECURSIVE_") else {
        return false;
    };
    if NON_CREDENTIAL_EXCEPTIONS.contains(&upper.as_str()) {
        return false;
    }
    rest.split('_')
        .any(|segment| matches!(segment, "KEY" | "KEYS" | "SECRET" | "TOKEN" | "AUTH"))
}

/// Credential variables that a feature compiled into this binary re-reads
/// from the environment *after* startup, so the startup scrub must leave
/// them alone. Each entry names its reader; drop the entry when that reader
/// takes the value from `Config` instead.
fn is_deferred_consumer(upper_name: &str) -> bool {
    // `memory::default_backends` → `OpenAiEmbedding::from_env` reads
    // RECURSIVE_EMBEDDING_API_KEY, falling back to RECURSIVE_API_KEY, every
    // time a tool registry is built (per session under `recursive http`).
    #[cfg(feature = "vector-memory")]
    if matches!(
        upper_name,
        "RECURSIVE_API_KEY" | "RECURSIVE_EMBEDDING_API_KEY"
    ) {
        return true;
    }
    // `E2bConfig::from_env` reads RECURSIVE_E2B_API_KEY every time the
    // microVM tool set is built (per session under `recursive http`).
    #[cfg(feature = "e2b-sandbox")]
    if upper_name == "RECURSIVE_E2B_API_KEY" {
        return true;
    }
    let _ = upper_name;
    false
}

/// What [`scrub_process_env`] did, for logging and tests. Names only —
/// never values.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScrubReport {
    /// Variables removed from the live environment, sorted.
    pub removed: Vec<String>,
    /// Variables that matched [`is_process_credential_env`] but were kept
    /// for a later reader (`keep_prefixes` or a compiled-in deferred
    /// consumer), sorted.
    pub retained: Vec<String>,
}

/// Issue #150 (defence in depth): remove this process's own credential
/// variables from the live environment.
///
/// Call it only after every reader of those variables has run —
/// `Config::from_env` for the upstream key, the HTTP router build for the
/// inbound auth keys. `keep_prefixes` lets a caller defer a group whose
/// reader has not run yet (see [`HTTP_AUTH_ENV_PREFIX`]); a second call
/// with an empty `keep_prefixes` picks them up later.
///
/// This edits only the libc `environ` array, so it is **not** the fix for
/// `/proc/<pid>/environ` — that is [`harden_process`]. What it does buy:
/// children spawned without issue #89's scrub (MCP stdio servers, `git`
/// helpers, the `run_code` runner) no longer inherit the credentials, and
/// crash reports or `environ` dumps in logs no longer carry them.
///
/// Must run before any task that reads the environment is spawned —
/// `std::env::remove_var` is process-global and racy against concurrent
/// `getenv`. The CLI calls it in the same startup window it already uses
/// for `set_var("RECURSIVE_SANDBOX")`.
pub fn scrub_process_env(keep_prefixes: &[&str]) -> ScrubReport {
    let mut report = ScrubReport::default();
    for (name_os, _) in std::env::vars_os() {
        let name = name_os.to_string_lossy().into_owned();
        if !is_process_credential_env(&name) {
            continue;
        }
        let upper = name.to_ascii_uppercase();
        let kept_by_caller = keep_prefixes.iter().any(|p| upper.starts_with(p));
        if kept_by_caller || is_deferred_consumer(&upper) {
            report.retained.push(name);
            continue;
        }
        std::env::remove_var(&name_os);
        report.removed.push(name);
    }
    report.removed.sort();
    report.retained.sort();
    if !report.removed.is_empty() {
        tracing::debug!(
            removed = ?report.removed,
            "hardening: dropped consumed credential variables from the process environment"
        );
    }
    if !report.retained.is_empty() {
        tracing::debug!(
            retained = ?report.retained,
            "hardening: credential variables kept for a reader that has not run yet"
        );
    }
    report
}

// ── Issue #226: clean-environment re-exec (unix except Linux) ───────────
//
// On Linux `harden_process` hides the exec-time environment block in the
// kernel. macOS has no such switch, and KERN_PROCARGS2 hands that block to
// any same-uid process — so the only remaining fix is to not have the
// credentials in the block at all: re-execute this binary with an
// environment that never contained them.
//
// The naive version — exec with the post-scrub environment — fails in both
// directions: variables the scrub *retained* for a later reader
// (`RECURSIVE_HTTP_AUTH_*` before the router build, the vector-memory /
// e2b-sandbox keys re-read per session) would ride the whitelist straight
// back into the new exec-time block, while variables the scrub *removed*
// (`RECURSIVE_API_KEY` in default builds, consumed into `Config`) would be
// gone entirely, so the re-executed process could not rebuild its config.
// Both are solved the way ssh-agent and sudo pass secrets across exec:
// the whitelist excludes every process-credential variable, and the
// pre-scrub values travel over a pipe whose read end survives `execve`
// (non-CLOEXEC) while the write end dies with the parent (CLOEXEC). The
// child restores them with `set_var` as its very first act — and `set_var`
// only touches the libc `environ` array, never the kernel block
// KERN_PROCARGS2 reads, so the credentials are functional but invisible.
//
// The read end is closed microseconds later, before this process can spawn
// anything, so no tool child ever holds it. The two reserved variables
// (`REEXEC_SENTINEL_VAR`, `REEXEC_HANDOFF_FD_VAR`) carry no secrets, are
// excluded from the scrub predicate (see the predicate test), and never
// reach tool children (issue #89's child scrub drops every `RECURSIVE_*`).

/// Environment variable that marks a process as *already* the clean-env
/// re-execution of itself. The parent sets it in the exec environment
/// (after the startup scrub — it is not credential-shaped, so the scrub
/// cannot eat it); the child sees it and skips the re-exec, bounding the
/// mechanism at one exec. Operators can also preset it as a kill-switch —
/// which opts out of both the protection and the exposure WARN.
pub const REEXEC_SENTINEL_VAR: &str = "RECURSIVE_REEXEC_DONE";

/// Environment variable naming the inherited pipe fd the parent left the
/// [`CredentialHandoff`] blob on. Consumed by [`restore_reexec_env`] and
/// removed from the environment immediately after.
pub const REEXEC_HANDOFF_FD_VAR: &str = "RECURSIVE_REEXEC_HANDOFF_FD";

/// The process-credential variables of the *original* environment, captured
/// before the startup scrub erases them, so the re-executed process can
/// rebuild the same live environment its parent had. Names and values.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CredentialHandoff {
    vars: Vec<(std::ffi::OsString, std::ffi::OsString)>,
}

impl CredentialHandoff {
    /// Snapshot every variable matching [`is_process_credential_env`] —
    /// call it immediately before [`scrub_process_env`], which destroys the
    /// values. Off the re-exec platforms (Linux, Windows) this is empty:
    /// nothing there consumes a handoff.
    pub fn capture() -> Self {
        #[cfg(all(unix, not(target_os = "linux")))]
        {
            let mut vars: Vec<_> = std::env::vars_os()
                .filter(|(name, _)| is_process_credential_env(&name.to_string_lossy()))
                .collect();
            vars.sort_by(|a, b| a.0.cmp(&b.0));
            vars.dedup_by(|a, b| a.0 == b.0);
            Self { vars }
        }
        #[cfg(any(not(unix), target_os = "linux"))]
        {
            Self::default()
        }
    }

    /// `true` when no process-credential variables existed at capture time.
    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }

    /// Wire format for the handoff pipe: `name\0value\0` pairs. Environment
    /// names and values cannot contain NUL, so this is lossless. Unix only:
    /// the byte view of `OsStr` is a unix guarantee, and the pipe protocol
    /// has no consumer elsewhere.
    #[cfg(unix)]
    pub fn encode(&self) -> Vec<u8> {
        let mut blob = Vec::new();
        for (name, value) in &self.vars {
            use std::os::unix::ffi::OsStrExt as _;
            blob.extend_from_slice(name.as_os_str().as_bytes());
            blob.push(0);
            blob.extend_from_slice(value.as_os_str().as_bytes());
            blob.push(0);
        }
        blob
    }

    /// All-or-nothing inverse of [`CredentialHandoff::encode`]: a truncated
    /// or malformed blob restores *nothing* (a half-restored credential set
    /// would fail downstream in confusing ways; failing loudly here is
    /// observable).
    #[cfg(unix)]
    pub fn decode(blob: &[u8]) -> Result<Self, &'static str> {
        if blob.is_empty() {
            return Ok(Self::default());
        }
        let fields: Vec<&[u8]> = blob.split(|&b| b == 0).collect();
        // split() yields one trailing empty slice for a well-formed blob
        // (every field is NUL-terminated); anything else is truncation.
        let fields = match fields.split_last() {
            Some(([], rest)) => rest,
            _ => return Err("truncated handoff blob"),
        };
        if fields.len() % 2 != 0 {
            return Err("truncated handoff blob");
        }
        use std::os::unix::ffi::OsStringExt as _;
        let mut vars = Vec::with_capacity(fields.len() / 2);
        for pair in fields.chunks(2) {
            if pair[0].is_empty() {
                return Err("empty variable name in handoff blob");
            }
            vars.push((
                std::ffi::OsString::from_vec(pair[0].to_vec()),
                std::ffi::OsString::from_vec(pair[1].to_vec()),
            ));
        }
        Ok(Self { vars })
    }
}

/// What [`reexec_clean_env`] decided. `exec` never returns, so there is no
/// "executed" arm — on success this function replaces the process image and
/// the *child* reports [`ReExecOutcome::AlreadyCleanEnv`] when it reaches
/// the same call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReExecOutcome {
    /// [`REEXEC_SENTINEL_VAR`] present: this process *is* the clean-env
    /// re-exec. The exec-time environment block carries no process
    /// credentials. The mechanism re-executes at most once.
    AlreadyCleanEnv,
    /// Linux: `harden_process` already hid the block with
    /// `prctl(PR_SET_DUMPABLE, 0)`; re-executing would add nothing.
    CoveredByKernelMechanism,
    /// Windows: no `execve` semantics to re-execute with (#207 convention).
    /// The PEB exposure documented at #150 stands.
    NoExecveSemantics,
    /// No process-credential variables existed at startup, so the exec-time
    /// block carries none of ours and re-executing would buy nothing.
    NotNeeded,
    /// The re-exec was attempted but failed before (or at) `exec`. The
    /// original process continues fail-open with its exec-time environment
    /// still readable; [`ReExecOutcome::report`] says so at WARN.
    Failed {
        /// Which step failed (`current_exe` / `pipe` / `handoff-write` /
        /// `exec`).
        stage: &'static str,
        /// Raw `errno`, when the failure came from the OS.
        errno: Option<i32>,
    },
}

impl ReExecOutcome {
    /// `true` when process-credential variables cannot be read out of this
    /// process's exec-time environment block — because it is clean (we are
    /// the re-exec), because the kernel hides it (Linux `prctl`), or because
    /// none were present to begin with. `false` on Windows and after a
    /// failed re-exec, where [`HardenOutcome::warn_if_exposed`] still
    /// applies.
    pub fn is_applied(&self) -> bool {
        matches!(
            self,
            ReExecOutcome::AlreadyCleanEnv
                | ReExecOutcome::CoveredByKernelMechanism
                | ReExecOutcome::NotNeeded
        )
    }

    /// Emit the outcome through `tracing`. Call once logging is up, exactly
    /// once per boot (owner ruling: the consumer states its position
    /// explicitly).
    pub fn report(&self) {
        match self {
            ReExecOutcome::AlreadyCleanEnv => {
                tracing::info!(
                    os = std::env::consts::OS,
                    "hardening: running as the clean-environment re-exec — the \
                     exec-time environment block carries no process credentials \
                     (issue #226)"
                );
            }
            ReExecOutcome::CoveredByKernelMechanism => {
                tracing::debug!(
                    "hardening: clean-env re-exec unnecessary on linux — \
                     prctl(PR_SET_DUMPABLE, 0) already hides the exec-time \
                     environment (issue #226)"
                );
            }
            ReExecOutcome::NoExecveSemantics => {
                tracing::debug!(
                    os = std::env::consts::OS,
                    "hardening: no clean-env re-exec on this platform (no execve \
                     semantics, #207) — exec-time environment exposure stands as \
                     documented at issue #150"
                );
            }
            ReExecOutcome::NotNeeded => {
                tracing::debug!(
                    "hardening: no process-credential variables at startup — \
                     clean-env re-exec not needed (issue #226)"
                );
            }
            ReExecOutcome::Failed { stage, errno } => {
                tracing::warn!(
                    stage = stage,
                    errno = ?errno,
                    "hardening: clean-environment re-exec failed; continuing in \
                     the original process, whose exec-time environment is still \
                     readable by same-uid processes (issue #226)"
                );
            }
        }
    }
}

/// Issue #226 (non-Linux unix): re-execute this binary with an environment
/// that carries no process-credential variables, handing the captured
/// [`CredentialHandoff`] to the new image over an inherited pipe fd.
///
/// Call it after config parsing and the startup scrub (the handoff must be
/// captured *before* the scrub) and before any listener or server starts —
/// at that point there are no sockets or other fds to reason about. On
/// success this function **does not return**: `execve` replaces the image
/// in-place (same pid, so supervisors and `$PPID` accounting are
/// unaffected) and startup restarts from the top of `main`. The child sees
/// [`REEXEC_SENTINEL_VAR`] and returns [`ReExecOutcome::AlreadyCleanEnv`]
/// here, bounding the mechanism at one exec.
///
/// Failures return [`ReExecOutcome::Failed`] and the original process
/// continues — fail-open with a WARN from [`ReExecOutcome::report`], the
/// same trade [`HardenOutcome::report`] documents for a failed `prctl`.
///
/// # Handoff protocol
///
/// 1. `pipe(2)`; the write end gets `FD_CLOEXEC` (dies with the parent at
///    exec), the read end keeps surviving (inherited by the new image).
/// 2. The blob (`name\0value\0` pairs) is written non-blocking — a blob too
///    big for the pipe buffer aborts the re-exec instead of deadlocking
///    before a reader exists.
/// 3. `execve(current_exe(), original argv, whitelist)` where the whitelist
///    is the current (post-scrub) environment minus every
///    [`is_process_credential_env`] match and minus the two reserved
///    variables, plus the sentinel and the handoff fd number.
/// 4. The child's [`restore_reexec_env`] — its first startup act — drains
///    the fd and `set_var`s the variables back into the live environment.
#[must_use = "the re-exec outcome must be reported (owner ruling: the consumer states its position explicitly)"]
pub fn reexec_clean_env(handoff: &CredentialHandoff) -> ReExecOutcome {
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        if std::env::var_os(REEXEC_SENTINEL_VAR).is_some() {
            return ReExecOutcome::AlreadyCleanEnv;
        }
        if handoff.is_empty() {
            return ReExecOutcome::NotNeeded;
        }
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(e) => {
                return ReExecOutcome::Failed {
                    stage: "current_exe",
                    errno: e.raw_os_error(),
                }
            }
        };
        let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();

        // The handoff pipe. The write end must die at exec (CLOEXEC) so the
        // child sees EOF; the read end must survive it (the default — set
        // explicitly so a future FD_CLOEXEC default cannot silently break
        // the protocol).
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: `pipe` fills the two-element fd array; no memory unsafety.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return ReExecOutcome::Failed {
                stage: "pipe",
                errno: std::io::Error::last_os_error().raw_os_error(),
            };
        }
        let (read_fd, write_fd) = (fds[0], fds[1]);
        // SAFETY: plain fd flag manipulation on fds this function owns.
        unsafe {
            libc::fcntl(write_fd, libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(read_fd, libc::F_SETFD, 0);
            // Non-blocking: with no reader until exec, a blocking write of a
            // blob larger than the pipe buffer would deadlock this process.
            libc::fcntl(write_fd, libc::F_SETFL, libc::O_NONBLOCK);
        }

        let blob = handoff.encode();
        let mut written = 0usize;
        let mut write_failed: Option<std::io::Error> = None;
        while written < blob.len() {
            // SAFETY: `write` on an fd this function owns; the slice outlives
            // the call.
            let n = unsafe {
                libc::write(
                    write_fd,
                    blob[written..].as_ptr().cast(),
                    blob.len() - written,
                )
            };
            if n < 0 {
                write_failed = Some(std::io::Error::last_os_error());
                break;
            }
            written += n as usize;
        }
        // SAFETY: fd owned by this function.
        unsafe { libc::close(write_fd) };
        if let Some(e) = write_failed {
            // SAFETY: fd owned by this function; no child was exec'd, so the
            // read end has no reader either.
            unsafe { libc::close(read_fd) };
            return ReExecOutcome::Failed {
                stage: "handoff-write",
                errno: e.raw_os_error(),
            };
        }

        // Whitelist: the post-scrub environment minus credential variables
        // (the retained ones — removed ones are already gone) and minus the
        // reserved names, plus sentinel + handoff fd.
        let envs = reexec_child_envs(read_fd);

        tracing::info!(
            os = std::env::consts::OS,
            mechanism = "clean-environment re-exec",
            handoff_vars = handoff.vars.len(),
            "hardening: no kernel mechanism hides this process's exec-time \
             environment on this platform; re-executing self with a scrubbed \
             environment, credentials passed over an inherited fd (issue #226)"
        );

        use std::os::unix::process::CommandExt as _;
        let mut cmd = std::process::Command::new(&exe);
        if let Some(argv0) = argv.first() {
            cmd.arg0(argv0);
        }
        cmd.args(argv.iter().skip(1));
        cmd.env_clear().envs(envs);
        // `exec` only returns on failure — success replaces this image.
        let e = cmd.exec();
        // SAFETY: this process keeps running; the read end is now useless
        // (no exec happened, no reader will appear).
        unsafe { libc::close(read_fd) };
        ReExecOutcome::Failed {
            stage: "exec",
            errno: e.raw_os_error(),
        }
    }
    #[cfg(target_os = "linux")]
    {
        let _ = handoff;
        ReExecOutcome::CoveredByKernelMechanism
    }
    #[cfg(not(unix))]
    {
        let _ = handoff;
        ReExecOutcome::NoExecveSemantics
    }
}

/// The environment handed to the re-executed child: everything currently in
/// the environment except process-credential variables (retained ones —
/// removed ones the scrub already took) and the two reserved [`#226`]
/// variables, plus the sentinel and the handoff fd. Factored out pure-ish
/// (reads the live environment, mutates nothing) for tests.
#[cfg(all(unix, not(target_os = "linux")))]
fn reexec_child_envs(handoff_fd: libc::c_int) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    std::env::vars_os()
        .filter(|(name, _)| {
            let name = name.to_string_lossy();
            !is_process_credential_env(&name)
                && name != REEXEC_SENTINEL_VAR
                && name != REEXEC_HANDOFF_FD_VAR
        })
        .chain([
            (
                std::ffi::OsString::from(REEXEC_SENTINEL_VAR),
                std::ffi::OsString::from("1"),
            ),
            (
                std::ffi::OsString::from(REEXEC_HANDOFF_FD_VAR),
                std::ffi::OsString::from(handoff_fd.to_string()),
            ),
        ])
        .collect()
}

/// What [`restore_reexec_env`] did, for reporting once logging is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// No [`REEXEC_HANDOFF_FD_VAR`] in the environment: a normal exec, not a
    /// #226 re-execution. The expected arm everywhere except the re-exec'd
    /// child.
    Absent,
    /// The handoff blob was drained and its variables restored into the live
    /// environment (names only travel in logs, never values).
    Restored {
        /// How many variables were restored.
        count: usize,
    },
    /// The handoff was present but unusable (bad fd, timed out, malformed).
    /// The child continues **without** the credentials it was meant to
    /// receive — downstream config loading will surface the gap; the WARN
    /// from [`RestoreOutcome::report`] explains it.
    Failed {
        /// What went wrong, stable for grepping.
        reason: &'static str,
    },
}

impl RestoreOutcome {
    /// Emit the outcome through `tracing`. Call once logging is up — the
    /// restore itself runs before any subscriber exists, so this is the only
    /// observable trace of it.
    pub fn report(&self) {
        match self {
            RestoreOutcome::Absent => {
                tracing::trace!(
                    "hardening: no re-exec handoff in the environment — booting \
                     from a normal exec (issue #226)"
                );
            }
            RestoreOutcome::Restored { count } => {
                tracing::info!(
                    count = count,
                    "hardening: restored process-credential variables from the \
                     clean-env re-exec handoff — they are live in the libc \
                     environ only; the exec-time block a same-uid reader sees is \
                     clean (issue #226)"
                );
            }
            RestoreOutcome::Failed { reason } => {
                tracing::warn!(
                    reason = reason,
                    "hardening: the re-exec credential handoff could not be \
                     restored; continuing without those variables, so env-injected \
                     credentials may be missing downstream (issue #226)"
                );
            }
        }
    }
}

/// Child side of the #226 re-exec: drain the credential handoff fd (if any)
/// and restore the variables into the live environment.
///
/// Must run before anything reads the environment — make it the first
/// statement of the CLI's startup — because `Config::from_env`, clap's env
/// injection and the startup scrub all expect the environment the parent
/// had. `set_var` only edits the libc `environ` array, never the kernel's
/// exec-time block, so restoring here does not re-expose anything to
/// KERN_PROCARGS2 readers.
///
/// The handoff is consumed: the fd is read to EOF (the parent's CLOEXEC
/// write end died at exec), closed, and [`REEXEC_HANDOFF_FD_VAR`] removed —
/// a second call is a no-op.
pub fn restore_reexec_env() -> RestoreOutcome {
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        use std::time::{Duration, Instant};

        let Some(fd_os) = std::env::var_os(REEXEC_HANDOFF_FD_VAR) else {
            return RestoreOutcome::Absent;
        };
        // Consume-once: whatever happens below, a repeated call must not
        // re-read a stale fd number.
        // SAFETY: process-global env mutation during single-threaded startup,
        // the same window the CLI already uses for `RECURSIVE_SANDBOX`.
        unsafe { std::env::remove_var(REEXEC_HANDOFF_FD_VAR) };
        let Ok(fd) = fd_os.to_string_lossy().parse::<libc::c_int>() else {
            return RestoreOutcome::Failed {
                reason: "handoff fd is not a valid descriptor number",
            };
        };
        if fd < 0 {
            return RestoreOutcome::Failed {
                reason: "handoff fd is negative",
            };
        }

        // Non-blocking read with a deadline: the write end is CLOEXEC, so EOF
        // is guaranteed once the parent is gone — but a bogus fd pointing at
        // something that never EOFs (a terminal, an unfilled socket) must
        // time out instead of hanging startup.
        // SAFETY: fd flag manipulation on a descriptor this function is
        // about to read and close.
        unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) };
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut blob: Vec<u8> = Vec::new();
        let mut read_failed: Option<&'static str> = None;
        loop {
            let mut chunk = [0u8; 4096];
            // SAFETY: `read` into a local buffer on an fd this function owns.
            let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
            if n > 0 {
                blob.extend_from_slice(&chunk[..n as usize]);
                if blob.len() > 256 * 1024 {
                    read_failed = Some("handoff blob exceeds 256 KiB");
                    break;
                }
                continue;
            }
            if n == 0 {
                break; // EOF: the parent is gone, the blob is complete.
            }
            match std::io::Error::last_os_error().kind() {
                std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        read_failed = Some("handoff pipe did not reach EOF within 2s");
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                _ => {
                    read_failed = Some("handoff read failed");
                    break;
                }
            }
        }
        // SAFETY: fd owned by this function.
        unsafe { libc::close(fd) };
        if let Some(reason) = read_failed {
            return RestoreOutcome::Failed { reason };
        }
        let handoff = match CredentialHandoff::decode(&blob) {
            Ok(handoff) => handoff,
            Err(reason) => return RestoreOutcome::Failed { reason },
        };
        let count = handoff.vars.len();
        for (name, value) in &handoff.vars {
            // SAFETY: process-global env mutation during single-threaded
            // startup, before any reader of these variables runs.
            unsafe { std::env::set_var(name, value) };
        }
        RestoreOutcome::Restored { count }
    }
    #[cfg(any(not(unix), target_os = "linux"))]
    {
        RestoreOutcome::Absent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Non-Linux unix targets keep a compiling, callable re-exec arm; the
    /// sentinel makes it a pure decision (no pipe, no exec) so this runs
    /// safely in-process. Pins "sentinel present → no second exec" — the
    /// anti-storm bound of the whole mechanism.
    #[cfg(all(unix, not(target_os = "linux")))]
    #[test]
    fn reexec_skips_when_sentinel_present() {
        let _lock = crate::test_util::env_lock();
        let saved = std::env::var_os(REEXEC_SENTINEL_VAR);
        // SAFETY: process-global env mutation, serialised by `env_lock`.
        unsafe { std::env::set_var(REEXEC_SENTINEL_VAR, "1") };
        let outcome = reexec_clean_env(&CredentialHandoff::capture());
        // SAFETY: restoring under the same lock.
        match saved {
            Some(v) => unsafe { std::env::set_var(REEXEC_SENTINEL_VAR, v) },
            None => unsafe { std::env::remove_var(REEXEC_SENTINEL_VAR) },
        }
        assert_eq!(outcome, ReExecOutcome::AlreadyCleanEnv);
        assert!(outcome.is_applied());
    }

    /// Nothing to protect (no credential variables captured) → no exec at
    /// all: the mechanism must not double-exec every credential-free CLI
    /// invocation. Same pure-decision property as the sentinel arm.
    ///
    /// Holds the crate-wide env lock even though it never mutates env
    /// (#185/#222: read-only env tests lock too): `reexec_clean_env` reads
    /// [`REEXEC_SENTINEL_VAR`], and without the lock that read can land
    /// inside `reexec_skips_when_sentinel_present`'s transient set/restore
    /// window, flipping this test onto the `AlreadyCleanEnv` branch.
    #[cfg(all(unix, not(target_os = "linux")))]
    #[test]
    fn reexec_skips_when_no_credentials_captured() {
        let _lock = crate::test_util::env_lock();
        let outcome = reexec_clean_env(&CredentialHandoff::default());
        assert_eq!(outcome, ReExecOutcome::NotNeeded);
        assert!(outcome.is_applied());
    }

    /// Linux: the kernel mechanism already covers the block, so the re-exec
    /// arm reports itself as covered (ubuntu CI exercises this directly).
    /// Takes the env lock like its sibling arms: same entry function, and a
    /// future env read in any compiled arm must not reintroduce the race.
    #[cfg(target_os = "linux")]
    #[test]
    fn reexec_is_covered_by_prctl_on_linux() {
        let _lock = crate::test_util::env_lock();
        let outcome = reexec_clean_env(&CredentialHandoff::default());
        assert_eq!(outcome, ReExecOutcome::CoveredByKernelMechanism);
        assert!(outcome.is_applied());
    }

    /// Linux: even a preset handoff fd is ignored — the fd protocol only
    /// exists on platforms that actually re-exec, and a Linux process must
    /// never let a stray environment variable point it at an fd. (ubuntu CI
    /// exercises this directly.)
    #[cfg(target_os = "linux")]
    #[test]
    fn restore_is_absent_on_linux_even_with_a_preset_fd() {
        let _lock = crate::test_util::env_lock();
        let saved = std::env::var_os(REEXEC_HANDOFF_FD_VAR);
        // SAFETY: process-global env mutation, serialised by `env_lock`.
        unsafe { std::env::set_var(REEXEC_HANDOFF_FD_VAR, "0") };
        let outcome = restore_reexec_env();
        // SAFETY: restoring under the same lock.
        match saved {
            Some(v) => unsafe { std::env::set_var(REEXEC_HANDOFF_FD_VAR, v) },
            None => unsafe { std::env::remove_var(REEXEC_HANDOFF_FD_VAR) },
        }
        assert_eq!(outcome, RestoreOutcome::Absent);
        // And the capture is empty there: no reader would consume it.
        assert!(CredentialHandoff::capture().is_empty());
    }

    /// Windows: no execve semantics (#207) — a documented no-op that must
    /// NOT claim the exposure is closed (windows CI job compiles this).
    /// Takes the env lock like its sibling arms: same entry function, and a
    /// future env read in any compiled arm must not reintroduce the race.
    #[cfg(not(unix))]
    #[test]
    fn reexec_is_noop_without_execve_semantics() {
        let _lock = crate::test_util::env_lock();
        let outcome = reexec_clean_env(&CredentialHandoff::default());
        assert_eq!(outcome, ReExecOutcome::NoExecveSemantics);
        assert!(!outcome.is_applied());
    }

    /// The handoff captures exactly the predicate-matching variables with
    /// their values — knobs, third-party names and the reserved variables
    /// stay out.
    #[cfg(all(unix, not(target_os = "linux")))]
    #[test]
    fn handoff_capture_takes_predicate_matches_only() {
        let _lock = crate::test_util::env_lock();
        const CRED: &str = "RECURSIVE_ISSUE226CAP_KEY";
        const KNOB: &str = "RECURSIVE_ISSUE226CAP_MAX_TOKENS";
        const THIRD: &str = "ISSUE226CAPX_API_KEY";
        const SECOND_CRED: &str = "RECURSIVE_ISSUE226CAP_HTTP_AUTH_KEYS";
        let saved: Vec<(&str, Option<std::ffi::OsString>)> = [CRED, KNOB, THIRD, SECOND_CRED]
            .iter()
            .map(|k| (*k, std::env::var_os(k)))
            .collect();
        // SAFETY: process-global env mutation, serialised by `env_lock`.
        unsafe { std::env::set_var(CRED, "value-with=equals and ünïcode") };
        unsafe { std::env::set_var(KNOB, "4096") };
        unsafe { std::env::set_var(THIRD, "third-party") };
        unsafe { std::env::set_var(SECOND_CRED, "auth-keys") };

        let handoff = CredentialHandoff::capture();

        // SAFETY: restoring under the same lock.
        for (k, v) in saved {
            match v {
                Some(v) => unsafe { std::env::set_var(k, v) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
        assert_eq!(
            handoff.vars,
            vec![
                (
                    std::ffi::OsString::from(SECOND_CRED),
                    std::ffi::OsString::from("auth-keys")
                ),
                (
                    std::ffi::OsString::from(CRED),
                    std::ffi::OsString::from("value-with=equals and ünïcode")
                ),
            ],
            "sorted, name+value, predicate matches only"
        );
        assert!(!handoff.is_empty());
    }

    /// Encode/decode round-trips `=`, whitespace and non-ASCII values
    /// losslessly, and a truncated blob restores nothing (all-or-nothing).
    /// Pure bytes — runs on every unix (ubuntu CI included).
    #[cfg(unix)]
    #[test]
    fn handoff_blob_round_trips_and_rejects_truncation() {
        let handoff = CredentialHandoff {
            vars: vec![
                (
                    std::ffi::OsString::from("RECURSIVE_ISSUE226BLOB_KEY"),
                    std::ffi::OsString::from("value=with=equals and spaces"),
                ),
                (
                    std::ffi::OsString::from("RECURSIVE_ISSUE226BLOB_TOKEN"),
                    std::ffi::OsString::from(""),
                ),
            ],
        };
        let blob = handoff.encode();
        assert_eq!(CredentialHandoff::decode(&blob), Ok(handoff));
        assert_eq!(
            CredentialHandoff::decode(&blob[..blob.len() - 1]),
            Err("truncated handoff blob")
        );
        assert_eq!(
            CredentialHandoff::decode(b""),
            Ok(CredentialHandoff::default())
        );
        // A lone name with no value field is an odd field count.
        assert_eq!(
            CredentialHandoff::decode(b"name\0"),
            Err("truncated handoff blob")
        );
        // NUL-free env strings make `name\0value\0` unambiguous; an empty
        // *name* is corrupt beyond repair.
        assert_eq!(
            CredentialHandoff::decode(b"\0value\0"),
            Err("empty variable name in handoff blob")
        );
    }

    /// The child environment whitelist carries everything except
    /// credential-shaped variables and the reserved names, plus exactly one
    /// sentinel and one handoff-fd entry — a preset reserved name must not
    /// produce a duplicate.
    #[cfg(all(unix, not(target_os = "linux")))]
    #[test]
    fn reexec_child_envs_excludes_credentials_and_reserved_names() {
        let _lock = crate::test_util::env_lock();
        const CRED: &str = "RECURSIVE_ISSUE226ENV_KEY";
        const KNOB: &str = "RECURSIVE_ISSUE226ENV_MAX_TOKENS";
        let saved: Vec<(&str, Option<std::ffi::OsString>)> =
            [CRED, KNOB, REEXEC_SENTINEL_VAR, REEXEC_HANDOFF_FD_VAR]
                .iter()
                .map(|k| (*k, std::env::var_os(k)))
                .collect();
        // SAFETY: process-global env mutation, serialised by `env_lock`.
        unsafe { std::env::set_var(CRED, "secret") };
        unsafe { std::env::set_var(KNOB, "4096") };
        unsafe { std::env::set_var(REEXEC_SENTINEL_VAR, "preset-by-operator") };
        unsafe { std::env::set_var(REEXEC_HANDOFF_FD_VAR, "999") };

        let envs = reexec_child_envs(7);

        // SAFETY: restoring under the same lock.
        for (k, v) in saved {
            match v {
                Some(v) => unsafe { std::env::set_var(k, v) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
        let get = |name: &str| {
            envs.iter()
                .filter(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            get(CRED),
            Vec::<std::ffi::OsString>::new(),
            "credential excluded"
        );
        assert_eq!(
            get(KNOB),
            vec![std::ffi::OsString::from("4096")],
            "knob rides along"
        );
        assert_eq!(
            get(REEXEC_SENTINEL_VAR),
            vec![std::ffi::OsString::from("1")],
            "preset sentinel overridden by exactly one entry"
        );
        assert_eq!(
            get(REEXEC_HANDOFF_FD_VAR),
            vec![std::ffi::OsString::from("7")],
            "preset fd number overridden by exactly one entry"
        );
    }

    /// The restore leg end-to-end over a real pipe: blob in, variables in
    /// the live environment, handoff fd consumed (closed + env var gone).
    #[cfg(all(unix, not(target_os = "linux")))]
    #[test]
    fn restore_reexec_env_drains_pipe_and_sets_variables() {
        let _lock = crate::test_util::env_lock();
        const CRED: &str = "RECURSIVE_ISSUE226RESTORE_KEY";
        let saved: Vec<(&str, Option<std::ffi::OsString>)> = [CRED, REEXEC_HANDOFF_FD_VAR]
            .iter()
            .map(|k| (*k, std::env::var_os(k)))
            .collect();
        // SAFETY: process-global env mutation, serialised by `env_lock`.
        unsafe { std::env::remove_var(CRED) };

        let handoff = CredentialHandoff {
            vars: vec![(
                std::ffi::OsString::from(CRED),
                std::ffi::OsString::from("restored-value"),
            )],
        };
        let blob = handoff.encode();
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: pipe(2) on a local array.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (read_fd, write_fd) = (fds[0], fds[1]);
        // SAFETY: write then close on fds this test owns.
        unsafe {
            assert_eq!(
                libc::write(write_fd, blob.as_ptr().cast(), blob.len()) as usize,
                blob.len()
            );
            libc::close(write_fd);
        }
        unsafe { std::env::set_var(REEXEC_HANDOFF_FD_VAR, read_fd.to_string()) };

        let outcome = restore_reexec_env();

        assert_eq!(outcome, RestoreOutcome::Restored { count: 1 });
        assert_eq!(std::env::var(CRED).ok().as_deref(), Some("restored-value"));
        assert!(
            std::env::var_os(REEXEC_HANDOFF_FD_VAR).is_none(),
            "handoff fd variable consumed"
        );
        // The fd itself is closed: reads fail with EBADF.
        // SAFETY: probing a closed descriptor.
        unsafe { assert_eq!(libc::read(read_fd, [0u8; 1].as_mut_ptr().cast(), 1), -1) };
        // A second call is a no-op (consume-once).
        assert_eq!(restore_reexec_env(), RestoreOutcome::Absent);

        // Failure branch: a preset fd number that is not open.
        // SAFETY: process-global env mutation, serialised by `env_lock`.
        unsafe { std::env::set_var(REEXEC_HANDOFF_FD_VAR, "2147483000") };
        assert!(
            matches!(restore_reexec_env(), RestoreOutcome::Failed { .. }),
            "bogus fd must land on Failed, not panic"
        );
        assert!(std::env::var_os(REEXEC_HANDOFF_FD_VAR).is_none());

        // SAFETY: restoring under the same lock.
        for (k, v) in saved {
            match v {
                Some(v) => unsafe { std::env::set_var(k, v) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
    }

    /// Both reports are total (no panic on any arm) and the
    /// degradation-relevant arms emit at the right level: the child's
    /// already-clean line is the INFO a deployment greps for, a failed
    /// re-exec must WARN (owner ruling: every degradation is observable).
    #[test]
    fn reexec_and_restore_reporting_are_total_and_graded() {
        for outcome in [
            ReExecOutcome::AlreadyCleanEnv,
            ReExecOutcome::CoveredByKernelMechanism,
            ReExecOutcome::NoExecveSemantics,
            ReExecOutcome::NotNeeded,
            ReExecOutcome::Failed {
                stage: "exec",
                errno: Some(2),
            },
        ] {
            outcome.report();
        }
        assert!(!ReExecOutcome::Failed {
            stage: "exec",
            errno: None
        }
        .is_applied());
        assert!(!ReExecOutcome::NoExecveSemantics.is_applied());

        for outcome in [
            RestoreOutcome::Absent,
            RestoreOutcome::Restored { count: 2 },
            RestoreOutcome::Failed {
                reason: "handoff read failed",
            },
        ] {
            outcome.report();
        }
    }

    /// The INFO/WARN content itself is pinned: `AlreadyCleanEnv` is the
    /// post-exec marker (integration test key), `Failed` names the stage,
    /// and the restore report says what happened without ever carrying a
    /// value.
    #[test]
    fn reexec_reports_name_mechanism_and_reason() {
        let applied = capture_logs(|| ReExecOutcome::AlreadyCleanEnv.report());
        assert!(applied.contains("INFO"), "{applied}");
        assert!(applied.contains("clean-environment re-exec"), "{applied}");
        assert!(
            applied.contains(&format!("os={:?}", std::env::consts::OS)),
            "{applied}"
        );

        let pre_exec = capture_logs(|| {
            tracing::info!(
                os = std::env::consts::OS,
                mechanism = "clean-environment re-exec",
                "hardening: no kernel mechanism hides this process's exec-time \
                 environment on this platform; re-executing self with a scrubbed \
                 environment, credentials passed over an inherited fd (issue #226)"
            )
        });
        assert!(
            pre_exec.contains("mechanism=\"clean-environment re-exec\""),
            "{pre_exec}"
        );

        let failed = capture_logs(|| {
            ReExecOutcome::Failed {
                stage: "handoff-write",
                errno: Some(35),
            }
            .report()
        });
        assert!(failed.contains("WARN"), "{failed}");
        assert!(failed.contains("stage=\"handoff-write\""), "{failed}");
        assert!(failed.contains("still readable"), "{failed}");

        let restored = capture_logs(|| RestoreOutcome::Restored { count: 3 }.report());
        assert!(restored.contains("INFO"), "{restored}");
        assert!(restored.contains("count=3"), "{restored}");
        assert!(restored.contains("handoff"), "{restored}");

        let restore_failed = capture_logs(|| {
            RestoreOutcome::Failed {
                reason: "handoff read failed",
            }
            .report()
        });
        assert!(restore_failed.contains("WARN"), "{restore_failed}");
        assert!(
            restore_failed.contains("continuing without"),
            "{restore_failed}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn harden_process_clears_dumpable_and_is_idempotent() {
        assert_eq!(harden_process(), HardenOutcome::Applied);

        // SAFETY: `PR_GET_DUMPABLE` takes no argument and only reads the
        // per-process dumpable flag.
        let dumpable = unsafe { libc::prctl(libc::PR_GET_DUMPABLE) };
        assert_eq!(dumpable, 0, "PR_SET_DUMPABLE did not take effect");

        // A second call must not panic, must report the same outcome and
        // must leave the flag set.
        assert_eq!(harden_process(), HardenOutcome::Applied);
        // SAFETY: as above.
        let again = unsafe { libc::prctl(libc::PR_GET_DUMPABLE) };
        assert_eq!(
            again, 0,
            "second harden_process() changed the dumpable flag"
        );
        assert!(HardenOutcome::Applied.is_applied());
    }

    /// Non-Linux targets keep a compiling, callable arm that reports
    /// itself as unsupported (macOS has no `/proc`; Windows neither) rather
    /// than pretending to have hardened anything.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn harden_process_reports_unsupported_off_linux() {
        assert_eq!(harden_process(), HardenOutcome::Unsupported);
        assert_eq!(harden_process(), HardenOutcome::Unsupported);
        assert!(!HardenOutcome::Unsupported.is_applied());
    }

    /// `report` / `warn_if_exposed` must not panic for any arm (totality).
    /// What each arm *emits* is asserted by the capture tests below.
    #[test]
    fn outcome_reporting_is_total() {
        let empty = ScrubReport::default();
        let some = ScrubReport {
            removed: vec!["RECURSIVE_API_KEY".into()],
            retained: vec![],
        };
        for outcome in [
            HardenOutcome::Applied,
            HardenOutcome::Failed { errno: 1 },
            HardenOutcome::Unsupported,
        ] {
            outcome.report();
            outcome.warn_if_exposed(&empty);
            outcome.warn_if_exposed(&some);
        }
        assert!(!HardenOutcome::Failed { errno: 1 }.is_applied());
    }

    #[test]
    fn predicate_matches_own_credentials_only() {
        // The three the issue names, plus the other RECURSIVE_ secrets the
        // audit found.
        for name in [
            "RECURSIVE_API_KEY",
            "RECURSIVE_HTTP_AUTH_KEYS",
            "RECURSIVE_HTTP_AUTH_JWT_SECRET",
            "RECURSIVE_HTTP_AUTH_KEY_OWNERS",
            "RECURSIVE_WEB_SEARCH_API_KEY",
            "RECURSIVE_WEB_SEARCH_JINA_KEY",
            "RECURSIVE_E2B_API_KEY",
            "RECURSIVE_EMBEDDING_API_KEY",
            "RECURSIVE_SOMETHING_TOKEN",
            "recursive_api_key", // Windows is case-insensitive
        ] {
            assert!(is_process_credential_env(name), "{name} should match");
        }
        // Config knobs in the same namespace that readers consult after
        // startup — `TOKENS` is not the `TOKEN` segment, `AUTHOR` is not
        // `AUTH`. The last two are #226's reserved re-exec variables: the
        // startup scrub must leave the sentinel and the handoff fd alone.
        for name in [
            "RECURSIVE_MAX_TOKENS",
            "RECURSIVE_HOME",
            "RECURSIVE_SANDBOX",
            "RECURSIVE_SESSIONS_DIR",
            "RECURSIVE_AUTHOR_NAME",
            "RECURSIVE_HTTP_AUTH_INSECURE_OK", // per-request switch, not a secret
            REEXEC_SENTINEL_VAR,
            REEXEC_HANDOFF_FD_VAR,
        ] {
            assert!(!is_process_credential_env(name), "{name} must not match");
        }
        // Third-party credentials are the *child* scrub's business
        // (issue #89); the service process must keep them for `${VAR}`
        // expansion / `env:` credential refs.
        for name in [
            "OPENAI_API_KEY",
            "DEEPSEEK_API_KEY",
            "GITHUB_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "SSH_AUTH_SOCK",
            "PATH",
        ] {
            assert!(!is_process_credential_env(name), "{name} must not match");
        }
    }

    /// Env-var checks are consolidated into ONE test (see `.dev/AGENTS.md`):
    /// `set_var` is process-global and `cargo test` runs tests in parallel.
    #[test]
    fn scrub_removes_consumed_credentials_and_honours_deferrals() {
        let _lock = crate::test_util::env_lock();

        // `RECURSIVE_WEB_SEARCH_API_KEY` is never deferred by any feature, so
        // it is the "always scrubbed" probe; `RECURSIVE_API_KEY` is deferred
        // only when `vector-memory` is compiled in; `RECURSIVE_E2B_API_KEY`
        // only when `e2b-sandbox` is (its `is_deferred_consumer` arm compiles
        // out under the mutants gate's default features — the assert below is
        // what kills that arm in a feature-enabled run, #223 run 39922642466).
        let probes = [
            "RECURSIVE_WEB_SEARCH_API_KEY",
            "RECURSIVE_API_KEY",
            "RECURSIVE_E2B_API_KEY",
            "RECURSIVE_HTTP_AUTH_KEYS",
            "RECURSIVE_HTTP_AUTH_JWT_SECRET",
            "RECURSIVE_HTTP_AUTH_INSECURE_OK",
            "RECURSIVE_MAX_TOKENS",
            "RECURSIVE_ISSUE150_BENIGN",
        ];
        let saved: Vec<(&str, Option<std::ffi::OsString>)> =
            probes.iter().map(|k| (*k, std::env::var_os(k))).collect();
        // SAFETY: process-global env mutation, serialised by `env_lock`.
        for k in probes {
            unsafe { std::env::set_var(k, "issue150-value") };
        }

        // Pass 1: the generic startup scrub with the HTTP auth group
        // deferred (what `recursive http` does).
        let first = scrub_process_env(&[HTTP_AUTH_ENV_PREFIX]);

        assert!(
            first
                .removed
                .contains(&"RECURSIVE_WEB_SEARCH_API_KEY".to_string()),
            "consumed credential not removed: {first:?}"
        );
        assert!(std::env::var_os("RECURSIVE_WEB_SEARCH_API_KEY").is_none());
        for kept in ["RECURSIVE_HTTP_AUTH_KEYS", "RECURSIVE_HTTP_AUTH_JWT_SECRET"] {
            assert!(
                first.retained.contains(&kept.to_string()),
                "{kept} should be retained for the router build: {first:?}"
            );
            assert!(std::env::var_os(kept).is_some(), "{kept} removed early");
        }
        if cfg!(feature = "vector-memory") {
            assert!(
                first.retained.contains(&"RECURSIVE_API_KEY".to_string()),
                "vector-memory re-reads RECURSIVE_API_KEY; it must be retained: {first:?}"
            );
            assert!(std::env::var_os("RECURSIVE_API_KEY").is_some());
        } else {
            assert!(
                first.removed.contains(&"RECURSIVE_API_KEY".to_string()),
                "without vector-memory RECURSIVE_API_KEY has no later reader: {first:?}"
            );
            assert!(std::env::var_os("RECURSIVE_API_KEY").is_none());
        }
        if cfg!(feature = "e2b-sandbox") {
            assert!(
                first
                    .retained
                    .contains(&"RECURSIVE_E2B_API_KEY".to_string()),
                "e2b-sandbox re-reads RECURSIVE_E2B_API_KEY; it must be retained: {first:?}"
            );
            assert!(std::env::var_os("RECURSIVE_E2B_API_KEY").is_some());
        } else {
            assert!(
                first.removed.contains(&"RECURSIVE_E2B_API_KEY".to_string()),
                "without e2b-sandbox RECURSIVE_E2B_API_KEY has no later reader: {first:?}"
            );
            assert!(std::env::var_os("RECURSIVE_E2B_API_KEY").is_none());
        }
        // Never touched: knobs, the per-request switch, unrelated names.
        for untouched in [
            "RECURSIVE_MAX_TOKENS",
            "RECURSIVE_HTTP_AUTH_INSECURE_OK",
            "RECURSIVE_ISSUE150_BENIGN",
        ] {
            assert_eq!(
                std::env::var(untouched).ok().as_deref(),
                Some("issue150-value"),
                "{untouched} must survive the scrub"
            );
            assert!(!first.removed.contains(&untouched.to_string()));
            assert!(!first.retained.contains(&untouched.to_string()));
        }

        // Pass 2: the router is built — nothing deferred any more.
        let second = scrub_process_env(&[]);
        for name in ["RECURSIVE_HTTP_AUTH_KEYS", "RECURSIVE_HTTP_AUTH_JWT_SECRET"] {
            assert!(
                second.removed.contains(&name.to_string()),
                "{name} should go on the second pass: {second:?}"
            );
            assert!(std::env::var_os(name).is_none());
        }
        assert_eq!(
            std::env::var("RECURSIVE_HTTP_AUTH_INSECURE_OK")
                .ok()
                .as_deref(),
            Some("issue150-value")
        );

        // Idempotent once clean.
        let third = scrub_process_env(&[]);
        assert!(third.removed.is_empty(), "{third:?}");

        // SAFETY: restoring under the same lock.
        for (k, v) in saved {
            match v {
                Some(v) => unsafe { std::env::set_var(k, v) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
    }

    // ── tracing capture harness (#223, CI run 39922642466) ─────────────────
    // The owner ruling "any degradation must be observable" makes the emitted
    // WARN itself the behaviour of `report` / `warn_if_exposed`, so the tests
    // below assert the events and their structured fields, not just that
    // nothing panics. `tracing::subscriber::with_default` scopes a
    // DEBUG-level fmt subscriber to the calling thread only: parallel-safe,
    // and it never touches any process-global subscriber.

    /// Append-only shared sink the capture subscriber formats into.
    #[derive(Clone)]
    struct LogCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogCapture {
        fn write(&mut self, chunk: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("log capture buffer")
                .extend_from_slice(chunk);
            Ok(chunk.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Run `f` with a thread-local DEBUG-level tracing subscriber and return
    /// everything it formatted (ANSI off, one event per line). The closure's
    /// own return value is discarded.
    fn capture_logs<T>(f: impl FnOnce() -> T) -> String {
        let buffer: std::sync::Arc<std::sync::Mutex<Vec<u8>>> = Default::default();
        let sink = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(move || LogCapture(sink.clone()))
            .finish();
        let _ignored = tracing::subscriber::with_default(subscriber, f);
        let out = buffer.lock().expect("log capture buffer").clone();
        String::from_utf8(out).expect("tracing output is utf-8")
    }

    /// The `Failed` arm's WARN **is** the fix's observable behaviour (owner
    /// ruling: any degradation must be observable). It must name the
    /// mechanism and the kernel's `errno`, both as the message and as
    /// structured fields, so a deployment can tell a seccomp refusal from an
    /// exotic kernel gap instead of silently accepting the gap.
    #[test]
    fn report_failed_emits_warn_naming_mechanism_and_errno() {
        let logs = capture_logs(|| HardenOutcome::Failed { errno: 1 }.report());
        assert!(logs.contains("WARN"), "expected a WARN event, got: {logs}");
        assert!(
            logs.contains("hardening: prctl(PR_SET_DUMPABLE, 0) failed"),
            "{logs}"
        );
        assert!(
            logs.contains("mechanism=\"prctl(PR_SET_DUMPABLE, 0)\""),
            "the structured mechanism field, not just the message: {logs}"
        );
        assert!(
            logs.contains("errno=1"),
            "the structured errno field: {logs}"
        );
    }

    /// The non-failed arms stay DEBUG (they are not degradations) but still
    /// say what ran and on which OS — `Unsupported` is the arm macOS and
    /// Windows production runs take, so the `os` field is what makes those
    /// logs greppable.
    #[test]
    fn report_applied_and_unsupported_emit_debug_with_os() {
        let applied = capture_logs(|| HardenOutcome::Applied.report());
        assert!(applied.contains("DEBUG"), "{applied}");
        assert!(
            applied.contains("prctl(PR_SET_DUMPABLE, 0) applied"),
            "{applied}"
        );

        let unsupported = capture_logs(|| HardenOutcome::Unsupported.report());
        assert!(unsupported.contains("DEBUG"), "{unsupported}");
        assert!(unsupported.contains("no kernel mechanism"), "{unsupported}");
        assert!(
            unsupported.contains(&format!("os={:?}", std::env::consts::OS)),
            "{unsupported}"
        );
    }

    /// `warn_if_exposed` on an exposed, unhid environment: one WARN carrying
    /// the *sum* count (`removed + retained` — the `+` → `-` mutant would
    /// underflow, or report 0 and go quiet) and the exposed variable *names*.
    #[test]
    fn warn_if_exposed_emits_one_warn_with_sum_count_and_names() {
        let scrub = ScrubReport {
            removed: vec!["RECURSIVE_API_KEY".into()],
            retained: vec![
                "RECURSIVE_HTTP_AUTH_KEYS".into(),
                "RECURSIVE_HTTP_AUTH_JWT_SECRET".into(),
            ],
        };
        let logs = capture_logs(|| HardenOutcome::Unsupported.warn_if_exposed(&scrub));
        assert!(logs.contains("WARN"), "{logs}");
        assert!(
            logs.contains("count=3"),
            "count is removed(1) + retained(2): {logs}"
        );
        assert!(
            logs.contains(
                "variables=[\"RECURSIVE_API_KEY\", \"RECURSIVE_HTTP_AUTH_KEYS\", \
                 \"RECURSIVE_HTTP_AUTH_JWT_SECRET\"]"
            ),
            "the WARN names every exposed variable: {logs}"
        );
        assert!(
            logs.contains(&format!("os={:?}", std::env::consts::OS)),
            "{logs}"
        );
    }

    /// Quiet by design in both clean directions: `Applied` means the exec-time
    /// copy is hidden (no degradation), and an empty scrub means there is
    /// nothing to leak. The empty-scrub row is what kills the `exposed == 0`
    /// → `!=` mutant, which would warn precisely when there is nothing to
    /// warn about and stay silent when there is.
    #[test]
    fn warn_if_exposed_is_quiet_when_applied_or_nothing_matched() {
        let some = ScrubReport {
            removed: vec!["RECURSIVE_API_KEY".into()],
            retained: vec![],
        };
        let applied = capture_logs(|| HardenOutcome::Applied.warn_if_exposed(&some));
        assert!(
            applied.is_empty(),
            "a hardened process has nothing to warn about: {applied:?}"
        );
        let nothing =
            capture_logs(|| HardenOutcome::Unsupported.warn_if_exposed(&ScrubReport::default()));
        assert!(
            nothing.is_empty(),
            "no credential variables, no exposure, no WARN: {nothing:?}"
        );
    }

    /// The two `scrub_process_env` DEBUG lines are the audit trail of what
    /// the startup scrub touched; each must fire exactly when its list is
    /// non-empty — the `delete !` mutants invert them into logging the empty
    /// scrub and going quiet on the real one. Env-mutating, so it runs under
    /// the shared env lock like every env test.
    #[test]
    fn scrub_debug_lines_fire_exactly_when_their_list_is_non_empty() {
        let _lock = crate::test_util::env_lock();
        const REMOVED: &str = "RECURSIVE_SCRUBLOG_REMOVED_KEY";
        const RETAINED: &str = "RECURSIVE_SCRUBLOG_KEEP_AUTH_KEYS";
        let saved: Vec<(&str, Option<std::ffi::OsString>)> = [REMOVED, RETAINED]
            .iter()
            .map(|k| (*k, std::env::var_os(k)))
            .collect();
        // SAFETY: process-global env mutation, serialised by `env_lock`.
        unsafe { std::env::set_var(REMOVED, "issue223-value") };
        unsafe { std::env::set_var(RETAINED, "issue223-value") };
        // The scrub removes every matching variable it is not told to keep —
        // including any the developer's shell exported. Snapshot those so the
        // restore below leaves the process environment exactly as it was.
        let foreign: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os()
            .filter(|(k, _)| {
                let k = k.to_string_lossy();
                is_process_credential_env(&k) && k != REMOVED && k != RETAINED
            })
            .collect();

        // Pass 1: one removal + one keep-prefix retention — both lines fire
        // and both carry the probe name.
        let first = capture_logs(|| scrub_process_env(&["RECURSIVE_SCRUBLOG_KEEP_"]));
        assert!(first.contains("DEBUG"), "{first}");
        assert!(
            first.contains("dropped consumed credential variables") && first.contains(REMOVED),
            "the removed line names the scrubbed probe: {first}"
        );
        assert!(
            first.contains("kept for a reader that has not run yet") && first.contains(RETAINED),
            "the retained line names the kept probe: {first}"
        );

        // Pass 2: retention dropped — the removed line fires again, the kept
        // line must not.
        let second = capture_logs(|| scrub_process_env(&[]));
        assert!(
            second.contains("dropped consumed credential variables"),
            "{second}"
        );
        assert!(
            !second.contains("kept for a reader that has not run yet"),
            "nothing was kept on the second pass: {second}"
        );

        // Pass 3: nothing matching left — neither line fires.
        let third = capture_logs(|| scrub_process_env(&[]));
        assert!(
            !third.contains("dropped consumed credential variables")
                && !third.contains("kept for a reader that has not run yet"),
            "a clean scrub must stay silent: {third}"
        );

        // SAFETY: restoring under the same lock; credentials of the outer
        // environment that the scrub removed go back too.
        for (k, v) in foreign {
            unsafe { std::env::set_var(&k, &v) };
        }
        for (k, v) in saved {
            match v {
                Some(v) => unsafe { std::env::set_var(k, v) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
    }
}
