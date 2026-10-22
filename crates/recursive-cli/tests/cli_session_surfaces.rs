//! `recursive sessions list` / `sessions delete` / `replay --resume-from`.
//!
//! These pin the user-visible branches that `main()` gates on helper
//! predicates, so a mutant flipping the predicate (e.g. `delete !` on
//! `if !has_sessions(total)`, or the `--resume-from` match guard) is caught
//! instead of silently reporting "Sessions (0):" for an empty root.

use std::path::Path;
use std::process::{Command, Output, Stdio};

struct Rig {
    home: tempfile::TempDir,
    workspace: tempfile::TempDir,
    sessions: tempfile::TempDir,
}

impl Rig {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().expect("home tempdir"),
            workspace: tempfile::tempdir().expect("workspace tempdir"),
            sessions: tempfile::tempdir().expect("sessions tempdir"),
        }
    }

    fn sessions_root(&self) -> &Path {
        self.sessions.path()
    }

    /// A `recursive` invocation with global flags before the subcommand, a
    /// cleared environment, and `RECURSIVE_SESSIONS_DIR` pointed at a temp
    /// root so both on-disk session formats are under test control.
    fn cmd(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_recursive"));
        cmd.env_clear();
        cmd.env("HOME", self.home.path());
        cmd.env("RECURSIVE_HOME", self.home.path());
        cmd.env("RECURSIVE_SESSIONS_DIR", self.sessions.path());
        // No provider should ever be contacted by these commands.
        cmd.env("RECURSIVE_RETRY_MAX", "0");
        if let Some(path) = std::env::var_os("PATH") {
            cmd.env("PATH", path);
        }
        cmd.current_dir(self.workspace.path());
        cmd.arg("--log").arg("error");
        cmd.arg("--workspace").arg(self.workspace.path());
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut cmd = self.cmd();
        cmd.args(args);
        cmd.output().expect("spawn recursive")
    }

    fn run_with_stdin(&self, args: &[&str], stdin: &str) -> Output {
        let mut cmd = self.cmd();
        cmd.args(args);
        cmd.stdin(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn recursive");
        {
            use std::io::Write;
            let mut pipe = child.stdin.take().expect("child stdin");
            pipe.write_all(stdin.as_bytes()).expect("write stdin");
        }
        child.wait_with_output().expect("wait for recursive")
    }
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// One legacy single-file session (`*.json` directly under the sessions root).
fn legacy_session(root: &Path, name: &str) -> std::path::PathBuf {
    let path = root.join(name);
    std::fs::write(&path, "{\"schema_version\":1}").expect("write legacy session");
    path
}

/// One JSONL session directory (`<slug>/<id>/.meta.json`), the newer layout.
fn jsonl_session(root: &Path, slug: &str, id: &str) -> std::path::PathBuf {
    let dir = root.join(slug).join(id);
    std::fs::create_dir_all(&dir).expect("mkdir session dir");
    std::fs::write(dir.join(".meta.json"), "{}").expect("write meta");
    std::fs::write(dir.join("transcript.jsonl"), "").expect("write transcript");
    dir
}

// ── sessions list ───────────────────────────────────────────────────────────

#[test]
fn sessions_list_reports_nothing_for_an_empty_root() {
    let rig = Rig::new();
    let out = rig.run(&["sessions", "list"]);
    let stdout = stdout_of(&out);

    assert!(
        out.status.success(),
        "sessions list failed: {:?}",
        stderr_of(&out)
    );
    assert!(
        stdout.contains("No sessions found in"),
        "an empty session root must report nothing found, got:\n{stdout}"
    );
    assert!(
        !stdout.contains("Sessions ("),
        "an empty session root must not print a session count, got:\n{stdout}"
    );
}

#[test]
fn sessions_list_counts_both_stored_formats() {
    let rig = Rig::new();
    legacy_session(rig.sessions_root(), "legacy.json");
    jsonl_session(rig.sessions_root(), "workspace-slug", "sess-1");

    let out = rig.run(&["sessions", "list"]);
    let stdout = stdout_of(&out);

    assert!(
        out.status.success(),
        "sessions list failed: {:?}",
        stderr_of(&out)
    );
    assert!(
        stdout.contains("Sessions (2):"),
        "one legacy + one JSONL session must count as 2, got:\n{stdout}"
    );
    assert!(
        !stdout.contains("No sessions found in"),
        "a non-empty root must not report `no sessions`, got:\n{stdout}"
    );
}

// ── sessions delete ────────────────────────────────────────────────────────

#[test]
fn sessions_delete_aborts_when_the_answer_is_not_yes() {
    let rig = Rig::new();
    let path = legacy_session(rig.sessions_root(), "to-delete.json");

    let out = rig.run_with_stdin(
        &["sessions", "delete", path.to_str().expect("utf8 path")],
        "n\n",
    );
    let stdout = stdout_of(&out);

    assert!(out.status.success(), "delete failed: {:?}", stderr_of(&out));
    assert!(
        stdout.contains("Aborted."),
        "a non-confirming answer must abort, got:\n{stdout}"
    );
    assert!(path.exists(), "aborted delete must keep the session file");
}

#[test]
fn sessions_delete_removes_the_session_on_yes() {
    let rig = Rig::new();
    let path = legacy_session(rig.sessions_root(), "to-delete.json");

    let out = rig.run_with_stdin(
        &["sessions", "delete", path.to_str().expect("utf8 path")],
        "yes\n",
    );
    let stdout = stdout_of(&out);

    assert!(out.status.success(), "delete failed: {:?}", stderr_of(&out));
    assert!(
        !stdout.contains("Aborted."),
        "`yes` must confirm the delete, got:\n{stdout}"
    );
    assert!(
        !path.exists(),
        "confirmed delete must remove the session file"
    );
}

#[test]
fn sessions_delete_with_force_skips_the_prompt() {
    let rig = Rig::new();
    let path = legacy_session(rig.sessions_root(), "to-delete.json");

    let out = rig.run(&[
        "sessions",
        "delete",
        path.to_str().expect("utf8 path"),
        "--force",
    ]);

    assert!(out.status.success(), "delete failed: {:?}", stderr_of(&out));
    assert!(
        !stderr_of(&out).contains("[y/N]"),
        "--force must not prompt, got:\n{}",
        stderr_of(&out)
    );
    assert!(
        !path.exists(),
        "--force delete must remove the session file"
    );
}

// ── replay --resume-from ───────────────────────────────────────────────────

fn transcript_fixture(rig: &Rig) -> std::path::PathBuf {
    let path = rig.workspace.path().join("transcript.json");
    std::fs::write(
        &path,
        r#"{"meta":{"saved_at":"2026-01-01T00:00:00Z","steps":1,"model":"test-model"},
            "messages":[{"role":"user","content":"seed"}]}"#,
    )
    .expect("write transcript fixture");
    path
}

#[test]
fn replay_resume_from_without_a_goal_is_rejected() {
    let rig = Rig::new();
    let transcript = transcript_fixture(&rig);

    let out = rig.run(&[
        "replay",
        transcript.to_str().expect("utf8 path"),
        "--resume-from",
        "1",
    ]);
    let stderr = stderr_of(&out);

    assert!(!out.status.success(), "replay without a goal must fail");
    assert!(
        stderr.contains("--resume-from requires a trailing <goal>"),
        "expected the missing-goal error, got:\n{stderr}"
    );
}

#[test]
fn replay_resume_from_with_a_goal_starts_the_run() {
    let rig = Rig::new();
    let transcript = transcript_fixture(&rig);

    // The provider endpoint is deliberately unreachable, so the run fails —
    // but it must fail *inside* the resumed run, never at the
    // `--resume-from requires a trailing <goal>` guard.
    let mut cmd = rig.cmd();
    cmd.arg("--api-base").arg("http://127.0.0.1:1");
    cmd.arg("--api-key").arg("sk-test-key");
    cmd.arg("--model").arg("test-model");
    cmd.arg("--provider").arg("openai");
    cmd.args([
        "replay",
        transcript.to_str().expect("utf8 path"),
        "--resume-from",
        "1",
        "continue",
        "the",
        "work",
    ]);
    let out = cmd.output().expect("spawn recursive");
    let stderr = stderr_of(&out);

    assert!(
        !stderr.contains("--resume-from requires a trailing <goal>"),
        "a trailing goal must satisfy the guard, got:\n{stderr}"
    );
    assert!(
        stderr.contains("resuming from 1 seeded message(s)"),
        "the resume banner should announce the seeded transcript, got:\n{stderr}"
    );
}

/// The transcript a crash during tool execution leaves behind:
/// `user → assistant(tool_calls)` with no `tool` result.
fn orphan_transcript_fixture(rig: &Rig) -> std::path::PathBuf {
    let path = rig.workspace.path().join("orphan-transcript.json");
    std::fs::write(
        &path,
        r#"{"meta":{"saved_at":"2026-01-01T00:00:00Z","steps":1,"model":"test-model"},
            "messages":[
              {"role":"user","content":"seed"},
              {"role":"assistant","content":"calling","tool_calls":[
                {"id":"tc-1","name":"Read","arguments":{"path":"note.txt"}}]}]}"#,
    )
    .expect("write orphan transcript fixture");
    path
}

/// Build a `replay <file> --resume-from 2 <goal>` invocation against an
/// unreachable provider: it fails *inside* the run, after the orphan scan.
fn replay_orphan_cmd(rig: &Rig, transcript: &Path, extra: &[&str]) -> Output {
    let mut cmd = rig.cmd();
    cmd.arg("--api-base").arg("http://127.0.0.1:1");
    cmd.arg("--api-key").arg("sk-test-key");
    cmd.arg("--model").arg("test-model");
    cmd.arg("--provider").arg("openai");
    let mut args = vec![
        "replay",
        transcript.to_str().expect("utf8 path"),
        "--resume-from",
        "2",
    ];
    args.extend_from_slice(extra);
    args.push("continue");
    cmd.args(args);
    cmd.output().expect("spawn recursive")
}

#[test]
fn replay_resume_from_answers_an_unpaired_seed_tail_by_default() {
    let rig = Rig::new();
    let transcript = orphan_transcript_fixture(&rig);

    // No --orphans flag: the unattended default (skip) must answer the tail
    // call instead of forwarding an unpaired transcript to the provider.
    let out = replay_orphan_cmd(&rig, &transcript, &[]);
    let stderr = stderr_of(&out);

    assert!(
        stderr.contains("1 incomplete tool call(s)"),
        "the orphan scan must run on the seed slice, got:\n{stderr}"
    );
    assert!(
        stderr.contains("--orphans=skip"),
        "the default policy must be the synthetic skip, got:\n{stderr}"
    );
    assert!(
        stderr.contains("resuming from 3 seeded message(s)"),
        "the synthetic result must be spliced into the seed (2 + 1), got:\n{stderr}"
    );
}

#[test]
fn replay_resume_from_orphans_abort_refuses_the_run() {
    let rig = Rig::new();
    let transcript = orphan_transcript_fixture(&rig);

    let out = replay_orphan_cmd(&rig, &transcript, &["--orphans", "abort"]);
    let stderr = stderr_of(&out);

    assert!(!out.status.success(), "--orphans=abort must fail the run");
    assert!(
        stderr.contains("refusing to continue"),
        "the explicit abort policy must refuse the orphaned seed, got:\n{stderr}"
    );
    assert!(
        !stderr.contains("resuming from"),
        "the refusal must land before the run starts, got:\n{stderr}"
    );
}

// ── issue #121: sessions --json / show cost+tail / agents liveness ──────────

/// A valid `.meta.json` payload (every required `SessionMeta` field present).
fn native_meta(id: &str, status: &str) -> String {
    format!(
        r#"{{"session_id":"{id}","goal":"goal-{id}","model":"m","provider":"p","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z","message_count":2,"status":"{status}","cost":{{"total_input_tokens":10,"total_output_tokens":5}}}}"#
    )
}

/// A native session directory with a valid meta and a 2-message transcript.
fn jsonl_session_with_meta(root: &Path, slug: &str, id: &str, meta: &str) -> std::path::PathBuf {
    let dir = root.join(slug).join(id);
    std::fs::create_dir_all(&dir).expect("mkdir session dir");
    std::fs::write(dir.join(".meta.json"), meta).expect("write meta");
    std::fs::write(
        dir.join("transcript.jsonl"),
        "{\"id\":\"msg_001\",\"role\":\"user\",\"content\":\"hello\",\"timestamp\":\"t\"}\n\
         {\"id\":\"msg_002\",\"role\":\"assistant\",\"content\":\"world\",\"timestamp\":\"t\"}\n",
    )
    .expect("write transcript");
    dir
}

#[test]
fn sessions_list_json_emits_the_persisted_meta() {
    let rig = Rig::new();
    jsonl_session_with_meta(
        rig.sessions_root(),
        "workspace-slug",
        "sess-json",
        &native_meta("sess-json", "completed"),
    );

    let out = rig.run(&["sessions", "list", "--json"]);
    assert!(
        out.status.success(),
        "sessions list --json failed: {:?}",
        stderr_of(&out)
    );
    let value: serde_json::Value =
        serde_json::from_str(stdout_of(&out).trim()).expect("list --json must emit JSON");
    assert_eq!(value["total"], 1);
    assert_eq!(value["sessions"][0]["format"], "jsonl");
    assert_eq!(value["sessions"][0]["meta"]["session_id"], "sess-json");
    assert_eq!(value["sessions"][0]["meta"]["message_count"], 2);
    assert_eq!(
        value["sessions"][0]["meta"]["status"], "completed",
        "the full SessionMeta must survive into the machine output"
    );
}

#[test]
fn sessions_show_json_reports_cost_and_the_full_transcript() {
    let rig = Rig::new();
    let dir = jsonl_session_with_meta(
        rig.sessions_root(),
        "slug",
        "sess-show",
        &native_meta("sess-show", "completed"),
    );

    let out = rig.run(&[
        "sessions",
        "show",
        dir.to_str().expect("utf8 path"),
        "--json",
    ]);
    assert!(
        out.status.success(),
        "sessions show --json failed: {:?}",
        stderr_of(&out)
    );
    let value: serde_json::Value =
        serde_json::from_str(stdout_of(&out).trim()).expect("show --json must emit JSON");
    assert_eq!(value["meta"]["session_id"], "sess-show");
    // The recorded cost must not be dropped (the pre-#121 gap).
    assert_eq!(value["meta"]["cost"]["total_input_tokens"], 10);
    assert_eq!(value["meta"]["cost"]["total_output_tokens"], 5);
    let transcript = value["transcript"]
        .as_array()
        .expect("transcript array in show --json");
    assert_eq!(transcript.len(), 2);
    assert_eq!(transcript[0]["type"], "message");
    assert_eq!(transcript[0]["message"]["content"], "hello");
}

#[test]
fn sessions_show_tail_limits_the_printed_window() {
    let rig = Rig::new();
    let dir = jsonl_session_with_meta(
        rig.sessions_root(),
        "slug",
        "sess-tail",
        &native_meta("sess-tail", "completed"),
    );

    let out = rig.run(&[
        "sessions",
        "show",
        dir.to_str().expect("utf8 path"),
        "--tail",
        "1",
    ]);
    let stdout = stdout_of(&out);
    assert!(
        out.status.success(),
        "show --tail failed: {:?}",
        stderr_of(&out)
    );
    assert!(
        stdout.contains("Transcript (last 1 of 2 entries):"),
        "--tail 1 must narrow the transcript to the last entry, got:\n{stdout}"
    );
    assert!(stdout.contains("world"), "the tail entry must be printed");
    assert!(
        !stdout.contains("hello"),
        "the dropped head entry must not be printed, got:\n{stdout}"
    );
}

/// A legacy single-file session with a 3-message transcript.
fn legacy_session_with_transcript(root: &Path, name: &str) -> std::path::PathBuf {
    let path = root.join(name);
    std::fs::write(
        &path,
        r#"{
  "schema_version": 1,
  "goal": "g",
  "model": "m",
  "provider": "p",
  "tool_registry_hash": "h",
  "steps_consumed": 0,
  "transcript": [
    {"role": "user", "content": "one"},
    {"role": "assistant", "content": "two"},
    {"role": "user", "content": "three"}
  ]
}"#,
    )
    .expect("write legacy session");
    path
}

#[test]
fn sessions_show_legacy_tail_reports_the_window_and_the_full_length() {
    let rig = Rig::new();
    let path = legacy_session_with_transcript(rig.sessions_root(), "legacy-tail.json");

    let out = rig.run(&[
        "sessions",
        "show",
        path.to_str().expect("utf8 path"),
        "--tail",
        "2",
    ]);
    let stdout = stdout_of(&out);
    assert!(
        out.status.success(),
        "legacy show --tail failed: {:?}",
        stderr_of(&out)
    );
    assert!(
        stdout.contains("transcript_len:  3"),
        "the printed length must stay the full file length, got:\n{stdout}"
    );
    assert!(
        stdout.contains("Transcript (last 2 of 3 entries):"),
        "legacy --tail must announce the window, got:\n{stdout}"
    );
    assert!(
        !stdout.contains("one"),
        "the dropped head entry must not be printed, got:\n{stdout}"
    );
}

#[test]
fn sessions_show_legacy_json_honors_tail() {
    let rig = Rig::new();
    let path = legacy_session_with_transcript(rig.sessions_root(), "legacy-json-tail.json");

    let out = rig.run(&[
        "sessions",
        "show",
        path.to_str().expect("utf8 path"),
        "--json",
        "--tail",
        "2",
    ]);
    assert!(
        out.status.success(),
        "legacy show --json --tail failed: {:?}",
        stderr_of(&out)
    );
    let value: serde_json::Value =
        serde_json::from_str(stdout_of(&out).trim()).expect("show --json must emit JSON");
    let transcript = value["session"]["transcript"]
        .as_array()
        .expect("legacy transcript array");
    assert_eq!(
        transcript.len(),
        2,
        "--json must apply --tail instead of returning the whole file"
    );
    assert_eq!(transcript[0]["content"], "two");
    assert_eq!(transcript[1]["content"], "three");
}

#[test]
fn agents_separates_live_stale_and_finished_sessions() {
    let rig = Rig::new();
    let root = rig.sessions_root();
    // Recorded active but no live lock: a crashed run (P0-1) → stale.
    jsonl_session_with_meta(
        root,
        "slug",
        "sess-stale",
        &native_meta("sess-stale", "active"),
    );
    // A lock held by another host is not ours to judge → live.
    let live = jsonl_session_with_meta(
        root,
        "slug",
        "sess-live",
        &native_meta("sess-live", "completed"),
    );
    std::fs::write(live.join(".lock"), "12345\ndefinitely-another-host\n0\n")
        .expect("write live lock");
    // Cleanly finished, no lock → not reported at all.
    jsonl_session_with_meta(
        root,
        "slug",
        "sess-done",
        &native_meta("sess-done", "completed"),
    );

    let out = rig.run(&["agents", "--json"]);
    assert!(
        out.status.success(),
        "agents --json failed: {:?}",
        stderr_of(&out)
    );
    let value: serde_json::Value =
        serde_json::from_str(stdout_of(&out).trim()).expect("agents --json must emit JSON");
    assert_eq!(
        value["count"], 2,
        "only in-flight-looking sessions are listed"
    );

    let agents = value["agents"].as_array().expect("agents array");
    let find = |id: &str| agents.iter().find(|a| a["session_id"] == id);
    assert_eq!(
        find("sess-stale").expect("stale session listed")["live"],
        false,
        "an active status with no lock is stale, not live"
    );
    assert_eq!(
        find("sess-live").expect("live session listed")["live"],
        true,
        "a cross-host lock holder is live"
    );
    assert!(
        find("sess-done").is_none(),
        "a finished, unheld session must not be listed"
    );
}

// ── issue #110: a failed run still closes its session envelope ──────────────

/// The only session directory under the sessions root.
fn find_session_dir(root: &Path) -> std::path::PathBuf {
    for slug in std::fs::read_dir(root).expect("read sessions root") {
        let slug = slug.expect("slug entry").path();
        if !slug.is_dir() {
            continue;
        }
        for session in std::fs::read_dir(&slug).expect("read slug dir") {
            let session = session.expect("session entry").path();
            if session.join(".meta.json").is_file() {
                return session;
            }
        }
    }
    panic!("no session directory found under {}", root.display());
}

/// `recursive run` against an unreachable provider, with `globals` placed
/// before the subcommand (e.g. an `--output-format`). The session writer is
/// created before the first LLM call, so the failure lands inside the run.
fn run_against_unreachable_provider(rig: &Rig, globals: &[&str]) -> Output {
    let mut cmd = rig.cmd();
    cmd.args(globals);
    cmd.arg("--api-base").arg("http://127.0.0.1:1");
    cmd.arg("--api-key").arg("sk-test-key");
    cmd.arg("--model").arg("test-model");
    cmd.arg("--provider").arg("openai");
    cmd.args(["run", "do", "something"]);
    cmd.output().expect("spawn recursive")
}

/// Assert the only session under the root closed as `crashed` with the error
/// text on `.meta.json`.
fn assert_session_crashed_with_error(rig: &Rig) -> std::path::PathBuf {
    let dir = find_session_dir(rig.sessions_root());
    let raw = std::fs::read(dir.join(".meta.json")).expect("read .meta.json");
    let meta: serde_json::Value =
        serde_json::from_slice(&raw).expect(".meta.json must be valid JSON");
    assert_eq!(
        meta["status"], "crashed",
        "a failed run must close its session envelope, got: {meta}"
    );
    assert!(
        meta["error"]
            .as_str()
            .map(|e| !e.is_empty())
            .unwrap_or(false),
        "the failure text must be carried onto the session, got: {meta}"
    );
    dir
}

/// A provider failure inside `run` must leave a *closed* session — status
/// `crashed` plus the error text on `.meta.json`, and `cost.json` on disk.
/// Before #110 the `Err` path returned before finalization, so every failed
/// run stayed `active`: a dead session indistinguishable from a live one.
#[test]
fn run_finalizes_the_session_as_crashed_when_the_provider_fails() {
    let rig = Rig::new();
    let out = run_against_unreachable_provider(&rig, &[]);
    assert!(
        !out.status.success(),
        "an unreachable provider must fail the run, got stdout:\n{}",
        stdout_of(&out)
    );

    let dir = assert_session_crashed_with_error(&rig);
    assert!(
        dir.join("cost.json").is_file(),
        "the run's spend record must still land as cost.json in {}",
        dir.display()
    );
}

/// The same contract in JSON mode. There the control session (and its stdin
/// demux) holds a writer clone that outlives the runtime, so finalization
/// must lock the shared writer instead of demanding unique ownership — the
/// old `Arc::into_inner` path refused and left these sessions `active`.
#[test]
fn run_finalizes_a_json_mode_session_as_crashed_when_the_provider_fails() {
    let rig = Rig::new();
    let out = run_against_unreachable_provider(&rig, &["--output-format", "stream-json"]);
    assert!(
        !out.status.success(),
        "an unreachable provider must fail the run, got stdout:\n{}",
        stdout_of(&out)
    );

    assert_session_crashed_with_error(&rig);
}

// ── migrate / gc-checkpoints / clean-checkpoints (issue #223) ───────────────
// These three commands had zero mutation coverage: `cmd_migrate` could return
// `Ok(())` moving nothing and both report-section guards (`!moved.is_empty()`
// / `!skipped.is_empty()`) could invert unnoticed, `gc-checkpoints` could skip
// the repo entirely, and `clean-checkpoints` could delete without consent.
// Drive the real binary so the printed sections and on-disk effects are both
// pinned. `user_workspace_dir`/`user_shadow_git_dir` are exported by the
// `recursive` crate the binary itself uses, so the tests verify the same
// locations the command writes.

fn git_on_path() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok()
}

/// Where the *child binary* puts the shadow repo: under the Rig's pinned home,
/// keyed by the workspace hash. Computed in-process from paths, never from
/// `user_shadow_git_dir` — that helper reads this process's env, and the test
/// process must not depend on (or set) `RECURSIVE_HOME` while sibling
/// integration tests run in parallel.
fn child_shadow_dir(rig: &Rig) -> std::path::PathBuf {
    rig.home
        .path()
        .join("workspaces")
        .join(recursive::workspace_hash(rig.workspace.path()))
        .join("shadow-git")
}

#[test]
#[cfg_attr(windows, ignore = "#224: migrate/Rig-spawn flow is unix-session-dir semantics; windows shadow-git fabricate differs (run 39700485676)")]
fn migrate_moves_legacy_state_and_prints_the_moved_section() {
    let rig = Rig::new();
    let legacy = rig.workspace.path().join(".recursive").join("sessions");
    std::fs::create_dir_all(&legacy).expect("mkdir legacy sessions");
    std::fs::write(legacy.join("old.json"), "{}").expect("write legacy session");

    let out = rig.run(&["migrate"]);
    let stdout = stdout_of(&out);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "migrate failed: {stderr}");
    assert!(
        stdout.contains("Moved:") && stdout.contains(".recursive/sessions"),
        "the moved section must list what moved, got:\n{stdout}"
    );
    assert!(
        !stderr.contains("warning:"),
        "nothing was skipped, so no warning was due, got:\n{stderr}"
    );
    assert!(
        !rig.workspace.path().join(".recursive").exists(),
        "the emptied legacy root must be removed"
    );
    // The whole legacy `sessions` dir relocates under the child's pinned home.
    let dest = rig
        .home
        .path()
        .join("workspaces")
        .join(recursive::workspace_hash(rig.workspace.path()))
        .join("sessions");
    assert!(
        dest.join("old.json").exists(),
        "the session must now live under {}",
        dest.display()
    );
}

#[test]
#[cfg_attr(windows, ignore = "#224: migrate/Rig-spawn flow is unix-session-dir semantics; windows shadow-git fabricate differs (run 39700485676)")]
fn migrate_reports_skips_when_destination_already_has_data() {
    let rig = Rig::new();
    // First run moves the legacy state under the user dir…
    let legacy_root = rig.workspace.path().join(".recursive");
    let legacy = legacy_root.join("sessions");
    std::fs::create_dir_all(&legacy).expect("mkdir legacy sessions");
    std::fs::write(legacy.join("old.json"), "{}").expect("write legacy session");
    let first = rig.run(&["migrate"]);
    assert!(
        first.status.success(),
        "first migrate failed: {:?}",
        stderr_of(&first)
    );

    // …then the same legacy path reappears and collides with the moved copy.
    std::fs::create_dir_all(&legacy).expect("re-mkdir legacy sessions");
    std::fs::write(legacy.join("old.json"), "{}").expect("re-write legacy session");
    let out = rig.run(&["migrate"]);
    let stdout = stdout_of(&out);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "second migrate failed: {stderr}");
    assert!(
        stdout.contains("Skipped (destination already exists):")
            && stdout.contains(".recursive/sessions"),
        "the skipped section must name the collision, got:\n{stdout}"
    );
    assert!(
        !stdout.contains("Moved:"),
        "nothing moved on the colliding run, got:\n{stdout}"
    );
    assert!(
        stderr.contains("warning:"),
        "a skipped item must raise the manual-merge warning, got:\n{stderr}"
    );
    assert!(
        legacy.join("old.json").exists(),
        "a skipped item stays put — migrate must not touch it"
    );
}

#[test]
fn gc_checkpoints_opens_and_prunes_the_shadow_repo() {
    if !git_on_path() {
        return;
    }
    let rig = Rig::new();
    let shadow = child_shadow_dir(&rig);
    assert!(!shadow.exists(), "a fresh workspace has no shadow repo yet");

    let out = rig.run(&["sessions", "gc-checkpoints"]);
    let stdout = stdout_of(&out);
    assert!(
        out.status.success(),
        "gc-checkpoints failed: {:?}",
        stderr_of(&out)
    );
    assert!(
        stdout.contains("Running git gc on shadow repo")
            && stdout.contains("Done. Shadow repo pruned."),
        "gc must announce start and completion, got:\n{stdout}"
    );
    assert!(
        shadow.exists(),
        "gc (via ShadowRepo::open) must leave the repo in place at {}",
        shadow.display()
    );
}

#[test]
fn clean_checkpoints_aborts_on_other_answers_deletes_on_yes_and_force() {
    if !git_on_path() {
        return;
    }
    let rig = Rig::new();
    let shadow = || child_shadow_dir(&rig);
    // Seed the repo: ShadowRepo::open creates the bare repo on demand.
    let seeded = rig.run(&["sessions", "gc-checkpoints"]);
    assert!(
        seeded.status.success(),
        "seeding gc failed: {:?}",
        stderr_of(&seeded)
    );
    assert!(
        shadow().exists(),
        "seed failed: no shadow repo at {}",
        shadow().display()
    );

    // Anything other than "y" aborts and keeps the repo.
    let out_n = rig.run_with_stdin(&["sessions", "clean-checkpoints"], "n\n");
    let stdout_n = stdout_of(&out_n);
    assert!(
        out_n.status.success(),
        "clean (n) failed: {:?}",
        stderr_of(&out_n)
    );
    assert!(
        stdout_n.contains("Aborted."),
        "a non-yes answer must abort, got:\n{stdout_n}"
    );
    assert!(
        shadow().exists(),
        "an aborted clean must not delete the repo"
    );

    // "y" consents — the repo is gone afterwards.
    let out_y = rig.run_with_stdin(&["sessions", "clean-checkpoints"], "y\n");
    let stdout_y = stdout_of(&out_y);
    assert!(
        out_y.status.success(),
        "clean (y) failed: {:?}",
        stderr_of(&out_y)
    );
    assert!(
        stdout_y.contains("Shadow-git repo deleted."),
        "a consenting clean must delete and say so, got:\n{stdout_y}"
    );
    assert!(
        !shadow().exists(),
        "a consenting clean must remove the repo"
    );

    // --force skips the prompt entirely (stdin is /dev/null here): the
    // deleted confirmation line proves the prompt was bypassed, not answered.
    let reseeded = rig.run(&["sessions", "gc-checkpoints"]);
    assert!(
        reseeded.status.success(),
        "reseed gc failed: {:?}",
        stderr_of(&reseeded)
    );
    let out_f = rig.run(&["sessions", "clean-checkpoints", "--force"]);
    let stdout_f = stdout_of(&out_f);
    assert!(
        out_f.status.success(),
        "clean --force failed: {:?}",
        stderr_of(&out_f)
    );
    assert!(
        stdout_f.contains("Shadow-git repo deleted."),
        "--force must delete without a prompt, got:\n{stdout_f}"
    );
    assert!(!shadow().exists(), "--force must remove the repo");
}
