//! Issue #150: the issue #89 env scrub is bypassable through
//! `/proc/<ppid>/environ`.
//!
//! `LocalTransport::exec_shell` scrubs credential-shaped variables out of
//! the *child's* environment (issue #89), but the service process kept its
//! own execve-time environment, and Linux mirrors that block at
//! `/proc/<pid>/environ`. Since the default tier spawns `/bin/sh` as a
//! direct child of the service process, a prompt-injected
//!
//! ```sh
//! tr '\0' '\n' < /proc/$PPID/environ
//! ```
//!
//! dumps every inbound credential and upstream key into the transcript.
//! `std::env::remove_var` cannot fix it (the kernel block is not the C
//! `environ` array); the fix is `recursive::hardening::harden_process()`
//! (`PR_SET_DUMPABLE=0`), which makes the kernel refuse the open to
//! same-uid children.
//!
//! Linux-only: macOS has no `/proc/<pid>/environ`, so the channel does not
//! exist there.
#![cfg(target_os = "linux")]

use recursive::tools::shell::RunShell;
use recursive::Tool;
use serde_json::json;

/// The issue's exact command: the shell spawned by the `Bash` tool is a
/// direct child of this process, so `$PPID` resolves to this process (the
/// "service" process in the threat model).
const DUMP_PARENT_ENVIRON: &str = "tr '\\0' '\\n' < /proc/$PPID/environ";

#[tokio::test]
async fn hardened_process_hides_its_environ_from_same_uid_children() {
    // `PR_SET_DUMPABLE` is ignored for a caller holding CAP_SYS_PTRACE, and
    // `/proc/<pid>/environ` stays root-readable, so a root service process
    // is not hidden by the fix. Skip rather than assert a false result.
    // SAFETY: `geteuid` is a trivial syscall wrapper with no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!(
            "SKIP: running as root — PR_SET_DUMPABLE does not hide \
             /proc/<pid>/environ from a root child"
        );
        return;
    }

    // ── Positive control ────────────────────────────────────────────────
    // Without hardening, a same-uid child CAN read its parent's
    // `/proc/<ppid>/environ`. Prove the channel exists so the negative
    // assertion below is not vacuous. The injected credential must be in
    // the *parent's* execve-time environment — a `set_var` after exec is
    // invisible to `/proc/<pid>/environ` — so it is passed to the outer
    // shell via `Command::env` and the grandchild reads it back.
    let control = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("/bin/sh -c 'cat /proc/$PPID/environ'")
        .env("RECURSIVE_API_KEY", "issue150-leak-me")
        .output()
        .expect("spawn positive-control shell");
    assert!(
        control.status.success(),
        "positive control shell failed: {control:?}"
    );
    let control_out = String::from_utf8_lossy(&control.stdout);
    assert!(
        control_out.contains("RECURSIVE_API_KEY=issue150-leak-me"),
        "positive control: an unhardened same-uid child could not read its \
         parent's environ — the channel this test targets is absent, so the \
         negative assertion would prove nothing. got: {control_out:?}"
    );

    // ── The fix ─────────────────────────────────────────────────────────
    // Harden *this* process (the one `$PPID` resolves to), then run the
    // issue's command through the real `Bash` tool.
    assert_eq!(
        recursive::hardening::harden_process(),
        recursive::hardening::HardenOutcome::Applied,
        "on a non-root Linux host prctl(PR_SET_DUMPABLE, 0) must succeed"
    );
    recursive::hardening::harden_process(); // idempotent

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let observation = RunShell::new(tmp.path())
        .execute(json!({ "command": DUMP_PARENT_ENVIRON }))
        .await
        .expect("RunShell::execute");

    assert!(
        !observation.contains("RECURSIVE_API_KEY=")
            && !observation.contains("RECURSIVE_HTTP_AUTH_KEYS=")
            && !observation.contains("RECURSIVE_HTTP_AUTH_JWT_SECRET="),
        "issue #150: credential env leaked to a same-uid child through \
         /proc/$PPID/environ:\n{observation}"
    );
    assert!(
        !observation.contains("exit: 0"),
        "issue #150: the child opened /proc/$PPID/environ (exit 0) — the \
         process was not hardened. observation:\n{observation}"
    );
}
