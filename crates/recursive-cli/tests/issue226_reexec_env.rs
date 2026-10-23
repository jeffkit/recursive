//! Issue #226: on macOS (and any non-Linux unix) `sysctl(KERN_PROCARGS2)` —
//! what `ps -Eww -p <pid>` reads — hands the execve-time environment of a
//! non-platform binary to every same-uid process, so the #89/#150 threat
//! (prompt injection → tool shell → read `$PPID`'s environment) still walks
//! off with credentials after `scrub_process_env`. #150's Linux fix
//! (`prctl(PR_SET_DUMPABLE, 0)`) has no macOS counterpart; the fix here is
//! a clean-environment re-exec of the binary with the credentials handed to
//! the new image over an inherited fd.
//!
//! These tests drive the real `recursive` binary and read its environment
//! the way the attacker would (`ps -Eww -p <pid>` from another process of
//! the same uid). The REPL (bare `recursive`) is the long-lived subject: it
//! blocks on the stdin pipe we hold open, so the process is alive while we
//! inspect it. `--log info` puts the three #226 lines on stderr, and the
//! child's "running as the clean-environment re-exec" line is the
//! synchronisation point — it can only be printed by the post-exec image.
//!
//! Visibility note for CI: the whole file compiles out on Linux (prctl is
//! the mechanism there, exercised by `tests/issue89_parent_environ.rs`) and
//! on Windows (no execve semantics, #207). The macOS evidence lives in the
//! local run that landed this test.

#![cfg(all(unix, not(target_os = "linux")))]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The credential probe. `RECURSIVE_API_KEY` is the issue's own example and
/// — in default builds, without `vector-memory` — exactly the variable the
/// startup scrub *removes*, so it exercises the hard half of the handoff:
/// the re-exec'd process must get it back to rebuild its config.
const PROBE_VAR: &str = "RECURSIVE_API_KEY";
const PROBE_VALUE: &str = "issue226-probe-KEYVALUE-7f3a";
/// A non-credential `RECURSIVE_*` knob that rides the whitelist: seeing it
/// in `ps -Eww` proves we read the right process's post-exec block rather
/// than an empty or foreign one.
const CONTROL_VAR: &str = "RECURSIVE_ISSUE226_CONTROL";
const CONTROL_VALUE: &str = "issue226-control-VALUE-7f3a";

const PARENT_MARKER: &str = "re-executing self with a scrubbed environment";
const RESTORE_MARKER: &str =
    "restored process-credential variables from the clean-env re-exec handoff";
const CHILD_MARKER: &str = "running as the clean-environment re-exec";
/// #150's standing exposure WARN — must be *silent* once the block is clean.
const EXPOSURE_WARN: &str = "nothing hides that copy on this platform";

fn base_cmd(home: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_recursive"));
    cmd.env_clear();
    cmd.env("HOME", home);
    cmd.env("RECURSIVE_HOME", home);
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    // The compact fmt layer wraps field names in ANSI sequences, which would
    // split literals like `count=1`; honour NO_COLOR so stderr is plain.
    cmd.env("NO_COLOR", "1");
    cmd.current_dir(home);
    cmd.arg("--log").arg("info");
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd
}

/// What a same-uid attacker sees: the exec-time environment block of `pid`.
/// Default `ps` output on purpose — `-o command=` drops the environment and
/// would make every "not visible" assertion pass vacuously.
fn ps_environment(pid: u32) -> String {
    let out = Command::new("ps")
        .args(["-Eww", "-p", &pid.to_string()])
        .output()
        .expect("spawn ps");
    assert!(out.status.success(), "ps -Eww -p {pid} failed: {out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Spawn the REPL and block until `marker` shows up on its stderr; returns
/// the still-running child and the stderr transcript up to and including
/// the marker. Kills the child and panics with the transcript on timeout or
/// early exit, so a broken boot reads as a test failure, not a hang.
fn spawn_repl_until(cmd: &mut Command, marker: &'static str) -> (Child, String) {
    cmd.stdin(Stdio::piped()); // held open → REPL blocks on read
    let mut child = cmd.spawn().expect("spawn recursive");
    let stderr = child.stderr.take().expect("stderr piped");
    let (tx, rx) = mpsc::channel::<String>();
    // Drain until EOF, never early: dropping the read end while the child
    // lives would EPIPE its next stderr write (the REPL banner) and kill it
    // before the ps probe below. Killing the child is what ends this thread.
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                continue; // receiver gone — keep draining regardless
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut transcript = String::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(line) => {
                let hit = line.contains(marker);
                transcript.push_str(&line);
                transcript.push('\n');
                if hit {
                    return (child, transcript);
                }
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("marker {marker:?} never appeared on stderr; transcript:\n{transcript}");
            }
        }
    }
}

/// The killer test: credential in, credential not readable from the
/// process's environment block, while the process is demonstrably alive,
/// demonstrably the same pid we spawned (execve keeps it) and demonstrably
/// the post-exec image (control knob and sentinel visible).
#[test]
fn reexec_hides_process_credentials_from_ps_environment() {
    let home = tempfile::tempdir().expect("home tempdir");
    let mut cmd = base_cmd(home.path());
    cmd.env(PROBE_VAR, PROBE_VALUE);
    cmd.env(CONTROL_VAR, CONTROL_VALUE);

    let (mut child, transcript) = spawn_repl_until(&mut cmd, CHILD_MARKER);
    let env_block = ps_environment(child.id());
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        !env_block.contains(PROBE_VALUE),
        "the probe credential is still readable from the exec-time block:\n{env_block}"
    );
    assert!(
        env_block.contains(&format!("{CONTROL_VAR}={CONTROL_VALUE}")),
        "control knob missing — we are not reading the live post-exec block:\n{env_block}"
    );
    assert!(
        env_block.contains("RECURSIVE_REEXEC_DONE=1"),
        "sentinel missing from the post-exec block:\n{env_block}"
    );

    // The three #226 lines, in order, and the mechanism ran exactly once:
    // the parent announced one re-exec, the child restored one variable and
    // then recognised itself as already clean (the anti-storm bound).
    assert_eq!(
        transcript.matches(PARENT_MARKER).count(),
        1,
        "exactly one re-exec announced:\n{transcript}"
    );
    assert!(
        transcript.contains(RESTORE_MARKER) && transcript.contains("count=1"),
        "the child must restore the one handed-off variable:\n{transcript}"
    );
    assert_eq!(transcript.matches(CHILD_MARKER).count(), 1, "{transcript}");
    let parent_at = transcript.find(PARENT_MARKER).expect("parent line");
    let restore_at = transcript.find(RESTORE_MARKER).expect("restore line");
    let child_at = transcript.find(CHILD_MARKER).expect("child line");
    assert!(
        parent_at < restore_at && restore_at < child_at,
        "parent → restore → already-clean ordering:\n{transcript}"
    );
    // The block is clean, so #150's exposure WARN would be a false alarm.
    assert!(
        !transcript.contains(EXPOSURE_WARN),
        "#150 exposure WARN must go quiet once the block is clean:\n{transcript}"
    );
    // Values never travel through logs.
    assert!(!transcript.contains(PROBE_VALUE), "{transcript}");
}

/// The restore leg against the real binary: a handoff blob on an inherited
/// pipe fd plus the sentinel is exactly what a #226 parent leaves behind.
/// The child must drain it, boot, recognise itself as already clean and
/// *not* exec again.
#[test]
fn child_restores_handoff_from_inherited_fd_and_does_not_reexec_again() {
    let home = tempfile::tempdir().expect("home tempdir");
    let blob = format!("{PROBE_VAR}\0{PROBE_VALUE}\0");
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: pipe(2) on a local array.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let (read_fd, write_fd) = (fds[0], fds[1]);
    // SAFETY: write the blob and close our write end so the child sees EOF;
    // the read end stays inheritable (no CLOEXEC) for the spawn below.
    unsafe {
        assert_eq!(
            libc::write(write_fd, blob.as_ptr().cast(), blob.len()) as usize,
            blob.len()
        );
        libc::close(write_fd);
    }

    let mut cmd = base_cmd(home.path());
    cmd.env("RECURSIVE_REEXEC_DONE", "1");
    cmd.env("RECURSIVE_REEXEC_HANDOFF_FD", read_fd.to_string());
    cmd.arg("tools");
    cmd.stdin(Stdio::null());
    let out = cmd.output().expect("run recursive tools");
    // SAFETY: our copy of the read end; the child had its own.
    unsafe { libc::close(read_fd) };

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tools failed: {out:?}");
    assert!(
        stderr.contains(RESTORE_MARKER) && stderr.contains("count=1"),
        "restore line missing:\n{stderr}"
    );
    assert!(stderr.contains(CHILD_MARKER), "{stderr}");
    assert!(
        !stderr.contains(PARENT_MARKER),
        "a process that already carries the sentinel must never exec again:\n{stderr}"
    );
    assert!(!stderr.contains(PROBE_VALUE), "{stderr}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.trim_start().starts_with('['),
        "tools should still print its JSON spec list:\n{stdout}"
    );
}

/// Scope marker, not an endorsement. #150's consumer audit keeps
/// third-party credential names (`OPENAI_API_KEY`, `GITHUB_TOKEN`, …) in
/// the live environment because `.mcp.json` `${VAR}` expansion, the
/// credential store's `env:` layer and `http_call`'s env-named auth read
/// them by name later; the #226 whitelist mirrors that, so they still land
/// in the exec-time block. Moving that boundary means redoing the consumer
/// audit — flip this assertion deliberately when you do.
#[test]
fn third_party_credentials_stay_in_exec_block_by_documented_scope() {
    const THIRD_PARTY_VAR: &str = "ISSUE226X_API_KEY";
    const THIRD_PARTY_VALUE: &str = "issue226-thirdparty-7f3a";
    let home = tempfile::tempdir().expect("home tempdir");
    let mut cmd = base_cmd(home.path());
    cmd.env(PROBE_VAR, PROBE_VALUE);
    cmd.env(THIRD_PARTY_VAR, THIRD_PARTY_VALUE);

    let (mut child, _transcript) = spawn_repl_until(&mut cmd, CHILD_MARKER);
    let env_block = ps_environment(child.id());
    let _ = child.kill();
    let _ = child.wait();

    assert!(!env_block.contains(PROBE_VALUE), "{env_block}");
    assert!(
        env_block.contains(&format!("{THIRD_PARTY_VAR}={THIRD_PARTY_VALUE}")),
        "third-party names ride the whitelist by #150's audit; if this fails the \
         scope moved — update the audit and this test together:\n{env_block}"
    );
}
