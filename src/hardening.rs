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
//! block. Two layers therefore work together:
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
//! * **macOS** — *open*. There is no `/proc`, but `sysctl(KERN_PROCARGS2)`
//!   (what `ps -Eww -p <pid>` / `ps eww` call) returns the execve-time
//!   environment of any same-uid process **unless the target is an Apple
//!   platform binary**. Measured on macOS 15.7: `/bin/sh` and `/bin/sleep`
//!   children show zero environment entries, a Homebrew `python3` or a
//!   freshly `cc`-built binary shows all of them — and ad-hoc
//!   `codesign -o runtime` (hardened runtime) does **not** change that. A
//!   cargo-built `recursive` is a non-platform binary, so on macOS the
//!   `$PPID` attack works with `ps -Eww -p $PPID`. Nothing here can hide
//!   that copy; [`HardenOutcome::warn_if_exposed`] says so at WARN whenever
//!   credentials were actually present. Closing it needs a clean-environment
//!   re-exec or a dedicated low-privilege uid for the service — tracked as
//!   #150 follow-up.
//! * **Windows** — *open* (same-user `ReadProcessMemory` of the PEB); same
//!   WARN. `harden_process` compiles to a no-op there and nothing in this
//!   module touches spawn behaviour.

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
/// for why macOS and Windows are *not* covered and what is exposed there.
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// `report` / `warn_if_exposed` must not panic for any arm; the WARN
    /// text itself is exercised by eye in the CLI, not asserted here.
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
        // `AUTH`.
        for name in [
            "RECURSIVE_MAX_TOKENS",
            "RECURSIVE_HOME",
            "RECURSIVE_SANDBOX",
            "RECURSIVE_SESSIONS_DIR",
            "RECURSIVE_AUTHOR_NAME",
            "RECURSIVE_HTTP_AUTH_INSECURE_OK", // per-request switch, not a secret
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
        // only when `vector-memory` is compiled in.
        let probes = [
            "RECURSIVE_WEB_SEARCH_API_KEY",
            "RECURSIVE_API_KEY",
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
}
