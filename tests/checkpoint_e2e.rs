//! End-to-end tests for the per-turn checkpoint + rewind flow.
//!
//! Uses MockProvider to drive AgentRuntime through real turns that
//! invoke real `WriteFile` / `RunShell` tools, then verifies that
//! `recursive::plan_rewind` + `apply_rewind` + `truncate_transcript_to_turn`
//! together restore both the workspace and the conversation state to
//! a chosen turn boundary.

use std::process::Command;
use std::sync::{Arc, Mutex};

use recursive::llm::{Completion, MockProvider, ToolCall};
use recursive::test_util::PinnedRecursiveHome;
use recursive::tools::{TouchedFiles, WriteFile};
use recursive::{
    apply_rewind, plan_rewind, truncate_transcript_to_turn, AgentRuntime, SessionWriter,
    ShadowRepo, ToolRegistry,
};
use serde_json::json;

/// Per-test override for `RECURSIVE_HOME`. Without this, the real
/// `~/.recursive` would be polluted (sessions, shadow-git) on every
/// e2e run. Wraps a [`PinnedRecursiveHome`] guard from the cross-module
/// `test_util` so that this test serialises against unit tests that
/// also touch `HOME`/`RECURSIVE_HOME` (e.g. `paths::tests`,
/// `migrate::tests`, `config::memory_home_dependent_tests`,
/// `tools::facts::test_i_scope_isolation`). The shared lock is the
/// only thing that prevents cross-binary env races inside one cargo
/// test process.
struct HomeOverride {
    _dir: tempfile::TempDir,
    _pin: PinnedRecursiveHome,
}

impl HomeOverride {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let pin = PinnedRecursiveHome::new(dir.path());
        Self {
            _dir: dir,
            _pin: pin,
        }
    }
}

fn has_git() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

fn write_file_call(id: &str, path: &str, contents: &str) -> Completion {
    Completion {
        content: format!("writing {path}"),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: "write_file".into(),
            arguments: json!({"path": path, "contents": contents}),
        }],
        finish_reason: Some("tool_calls".into()),
        usage: None,
        reasoning_content: None,
    }
}

fn final_completion(text: &str) -> Completion {
    Completion {
        content: text.into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }
}

#[tokio::test]
async fn rewind_undoes_turn_and_restores_files_and_transcript() {
    if !has_git() {
        eprintln!("git not available; skipping");
        return;
    }
    let _home = HomeOverride::new();
    let dir = tempfile::tempdir().unwrap();

    // Turn 0: agent writes a.txt then ends.
    // Turn 1: agent writes b.txt then ends.
    let llm = Arc::new(MockProvider::new(vec![
        write_file_call("c0", "a.txt", "v-turn-0"),
        final_completion("turn 0 done"),
        write_file_call("c1", "b.txt", "v-turn-1"),
        final_completion("turn 1 done"),
    ]));

    // Wire up tools with the touched-files collector.
    let touched = Arc::new(Mutex::new(TouchedFiles::new()));
    let tools = ToolRegistry::local()
        .register(Arc::new(WriteFile::new(dir.path())))
        .with_touched_files(touched.clone());

    let mut runtime = AgentRuntime::builder()
        .llm(llm)
        .tools(tools)
        .build()
        .unwrap();

    // Create a session writer + checkpoint plumbing, mirroring what
    // `recursive run` does in main.rs.
    let mut sw = SessionWriter::create(dir.path(), "e2e-goal", "mock", "mock").unwrap();
    let session_dir = sw.session_dir().to_path_buf();
    let log_path = session_dir.join("checkpoints.jsonl");
    let shadow = Arc::new(ShadowRepo::open(dir.path()).unwrap());
    runtime
        .enable_checkpoints(
            Arc::clone(&shadow),
            sw.session_id().to_string(),
            log_path.clone(),
            Some(touched),
        )
        .unwrap();

    // Mirror messages into the session transcript as the runtime
    // produces them. We append everything in the runtime's transcript
    // after each run() call.
    let mut prev = 0usize;
    let _o0 = runtime.run("please write a.txt").await.unwrap();
    for m in runtime.transcript().iter().skip(prev) {
        sw.append(m).unwrap();
    }
    prev = runtime.transcript().len();
    let _o1 = runtime.run("please write b.txt").await.unwrap();
    for m in runtime.transcript().iter().skip(prev) {
        sw.append(m).unwrap();
    }
    sw.finish("done").unwrap();

    // Both files should exist on disk now.
    let a_path = dir.path().join("a.txt");
    let b_path = dir.path().join("b.txt");
    assert_eq!(std::fs::read_to_string(&a_path).unwrap(), "v-turn-0");
    assert_eq!(std::fs::read_to_string(&b_path).unwrap(), "v-turn-1");

    // Two turn records were written.
    let recs = recursive::read_checkpoint_log(&log_path).unwrap();
    assert_eq!(recs.len(), 2);
    assert_eq!(recs[0].turn, 0);
    assert_eq!(recs[1].turn, 1);
    assert!(recs[0].touched_files.iter().any(|p| p == "a.txt"));
    assert!(recs[1].touched_files.iter().any(|p| p == "b.txt"));

    // ── Rewind to turn 1 ───────────────────────────────────────────
    let plan = plan_rewind(&log_path, 1).unwrap();
    let result = apply_rewind(&shadow, &log_path, &plan, false).expect("apply rewind");
    assert_eq!(result.dropped_turns, vec![1]);

    // a.txt unchanged; b.txt deleted (it didn't exist at turn 1's pre).
    assert_eq!(std::fs::read_to_string(&a_path).unwrap(), "v-turn-0");
    assert!(!b_path.exists(), "b.txt should be gone after rewind");

    // checkpoints.jsonl now only has turn 0.
    let recs = recursive::read_checkpoint_log(&log_path).unwrap();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].turn, 0);

    // Truncate transcript.jsonl to turn 1 (drops the user message of
    // turn 1 and everything that followed).
    let stats = truncate_transcript_to_turn(&session_dir, 1).unwrap();
    assert!(stats.dropped >= 2); // user "please write b.txt" + assistant final + any tool result
    assert!(stats.kept >= 2);

    // The remaining transcript should contain "please write a.txt" but
    // not "please write b.txt".
    let entries = recursive::SessionReader::load_transcript(&session_dir).unwrap();
    let texts: Vec<&str> = entries.iter().map(|e| e.content.as_str()).collect();
    assert!(
        texts.iter().any(|t| t.contains("please write a.txt")),
        "expected turn 0 user message to survive: {texts:?}"
    );
    assert!(
        !texts.iter().any(|t| t.contains("please write b.txt")),
        "turn 1 user message should be truncated: {texts:?}"
    );
}

#[tokio::test]
async fn rewind_does_not_touch_other_workspace_files() {
    // Verifies the per-session selective restore: a file the agent
    // never touched (an external file in the workspace) is not
    // affected by a rewind, even if it changed between snapshots.
    if !has_git() {
        return;
    }
    let _home = HomeOverride::new();
    let dir = tempfile::tempdir().unwrap();

    // Pre-existing untouched file.
    let untouched = dir.path().join("untouched.txt");
    std::fs::write(&untouched, "external-state").unwrap();

    let llm = Arc::new(MockProvider::new(vec![
        write_file_call("c0", "agent.txt", "v0"),
        final_completion("done"),
    ]));
    let touched = Arc::new(Mutex::new(TouchedFiles::new()));
    let tools = ToolRegistry::local()
        .register(Arc::new(WriteFile::new(dir.path())))
        .with_touched_files(touched.clone());
    let mut runtime = AgentRuntime::builder()
        .llm(llm)
        .tools(tools)
        .build()
        .unwrap();

    let sw = SessionWriter::create(dir.path(), "g", "mock", "mock").unwrap();
    let session_dir = sw.session_dir().to_path_buf();
    let log_path = session_dir.join("checkpoints.jsonl");
    let shadow = Arc::new(ShadowRepo::open(dir.path()).unwrap());
    runtime
        .enable_checkpoints(
            Arc::clone(&shadow),
            sw.session_id().to_string(),
            log_path.clone(),
            Some(touched),
        )
        .unwrap();

    runtime.run("write agent.txt").await.unwrap();

    // Simulate an external editor modifying untouched.txt after the
    // turn finished.
    std::fs::write(&untouched, "external-edit").unwrap();

    // Rewind to turn 0: agent.txt should be deleted, untouched.txt
    // should NOT be reverted to "external-state".
    let plan = plan_rewind(&log_path, 0).unwrap();
    apply_rewind(&shadow, &log_path, &plan, false).unwrap();
    assert!(!dir.path().join("agent.txt").exists());
    assert_eq!(
        std::fs::read_to_string(&untouched).unwrap(),
        "external-edit",
        "untouched.txt must not be reverted by rewind"
    );
}
