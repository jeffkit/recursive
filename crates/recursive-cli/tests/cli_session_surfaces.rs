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
