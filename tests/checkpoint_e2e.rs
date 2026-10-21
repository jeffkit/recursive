//! End-to-end tests for the per-turn checkpoint + rewind flow.
//!
//! Uses MockProvider to drive AgentRuntime through real turns that
//! invoke real `WriteFile` / `RunShell` tools, then verifies that
//! `recursive::plan_rewind` + `apply_rewind` + `truncate_transcript_to_turn`
//! together restore both the workspace and the conversation state to
//! a chosen turn boundary.

use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use recursive::llm::{Completion, MockProvider, ToolCall};
use recursive::test_util::PinnedRecursiveHome;
use recursive::tools::{TouchedFiles, WriteFile};
use recursive::{
    apply_rewind, plan_rewind, truncate_transcript_to_turn, AgentRuntime, SessionStatus,
    SessionWriter, ShadowRepo, ToolRegistry,
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
    /// `RECURSIVE_SESSIONS_DIR` is a hard override that beats
    /// `RECURSIVE_HOME`; without pinning it, a host-level value
    /// routes sessions into a shared directory and cross-test
    /// transcripts bleed into every assertion.
    _sessions: PinnedSessionsDir,
}

impl HomeOverride {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let pin = PinnedRecursiveHome::new(dir.path());
        let sessions = PinnedSessionsDir::new(dir.path().join("sessions"), &pin);
        Self {
            _dir: dir,
            _pin: pin,
            _sessions: sessions,
        }
    }
}

/// Guard that redirects `RECURSIVE_SESSIONS_DIR` for its lifetime.
/// Borrows the `PinnedRecursiveHome` to prove the global env lock is
/// already held (that guard must be created first).
struct PinnedSessionsDir {
    prev: Option<std::ffi::OsString>,
    _pin: std::marker::PhantomData<fn() -> PinnedRecursiveHome>,
}

impl PinnedSessionsDir {
    fn new(path: std::path::PathBuf, _held: &PinnedRecursiveHome) -> Self {
        let prev = std::env::var_os("RECURSIVE_SESSIONS_DIR");
        // SAFETY: the env lock is held by `_held`.
        unsafe {
            std::env::set_var("RECURSIVE_SESSIONS_DIR", path.as_os_str());
        }
        Self {
            prev,
            _pin: std::marker::PhantomData,
        }
    }
}

impl Drop for PinnedSessionsDir {
    fn drop(&mut self) {
        // SAFETY: still under the env lock held by the paired HomeOverride.
        unsafe {
            match self.prev.take() {
                Some(v) => std::env::set_var("RECURSIVE_SESSIONS_DIR", v),
                None => std::env::remove_var("RECURSIVE_SESSIONS_DIR"),
            }
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
            name: "Write".into(),
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

/// Issue #173: build a runtime wired to an *existing* session checkpoint
/// chain — the shape `resume` (`cli/resume.rs`) and AG-UI (`http/agui.rs`)
/// produce when they re-open a session on a brand-new runtime.
fn wired_runtime(
    dir: &Path,
    shadow: &Arc<ShadowRepo>,
    session_id: &str,
    log_path: &Path,
    completions: Vec<Completion>,
) -> (AgentRuntime, Arc<Mutex<TouchedFiles>>) {
    let touched = Arc::new(Mutex::new(TouchedFiles::new()));
    let tools = ToolRegistry::local()
        .register(Arc::new(WriteFile::new(dir)))
        .with_touched_files(touched.clone());
    let mut runtime = AgentRuntime::builder()
        .llm(Arc::new(MockProvider::new(completions)))
        .tools(tools)
        .build()
        .expect("runtime builds");
    runtime
        .enable_checkpoints(
            Arc::clone(shadow),
            session_id.to_string(),
            log_path.to_path_buf(),
            Some(touched.clone()),
        )
        .expect("enable_checkpoints");
    (runtime, touched)
}

/// Call the session's `checkpoint_save` tool the way the agent does.
async fn save_checkpoint(runtime: &AgentRuntime, message: &str) {
    let save = runtime
        .kernel()
        .tools()
        .get("checkpoint_save")
        .expect("checkpoint_save tool registered");
    save.execute(json!({ "message": message }))
        .await
        .expect("checkpoint_save succeeds");
}

/// The `turn` column of the session's checkpoint log, in file order.
fn log_turns(log_path: &Path) -> Vec<usize> {
    recursive::read_checkpoint_log(log_path)
        .expect("checkpoint log readable")
        .iter()
        .map(|r| r.turn)
        .collect()
}

/// Issue #173: a runtime rebuilt for `resume` must continue the session's
/// turn numbering instead of restarting at 0.
///
/// Pre-fix the resumed runtime's `turn_index` is 0, so the first
/// post-resume `checkpoint_save` writes a `turn` that duplicates a
/// pre-resume record — exactly what made `sessions rewind --to-turn N`
/// skip the post-resume changes.
#[tokio::test]
async fn resume_continues_turn_numbering() {
    if !has_git() {
        return;
    }
    let _home = HomeOverride::new();
    let dir = tempfile::tempdir().unwrap();
    let sw = SessionWriter::create(dir.path(), "e2e-resume-numbering", "mock", "mock").unwrap();
    let session_dir = sw.session_dir().to_path_buf();
    let log_path = session_dir.join("checkpoints.jsonl");
    let session_id = sw.session_id().to_string();
    let shadow = Arc::new(ShadowRepo::open(dir.path()).unwrap());

    // Pre-resume: three snapshots taken during turns 0/1/2 — the issue's
    // "会话在 turn 0-2 有快照" state.
    let (mut rt_a, _touched_a) = wired_runtime(
        dir.path(),
        &shadow,
        &session_id,
        &log_path,
        vec![
            write_file_call("c0", "a.txt", "a-v0"),
            final_completion("turn 0 done"),
            write_file_call("c1", "b.txt", "b-v1"),
            final_completion("turn 1 done"),
            write_file_call("c2", "c.txt", "c-v2"),
            final_completion("turn 2 done"),
        ],
    );
    save_checkpoint(&rt_a, "turn 0").await;
    rt_a.run("write a.txt").await.unwrap();
    save_checkpoint(&rt_a, "turn 1").await;
    rt_a.run("write b.txt").await.unwrap();
    save_checkpoint(&rt_a, "turn 2").await;
    rt_a.run("write c.txt").await.unwrap();
    drop(rt_a);
    assert_eq!(log_turns(&log_path), vec![0, 1, 2], "pre-resume log");

    // ── "resume": a brand-new runtime over the same session dir + log.
    let (rt_b, _touched_b) = wired_runtime(dir.path(), &shadow, &session_id, &log_path, vec![]);

    assert_eq!(
        rt_b.turn_index(),
        3,
        "resume must seed turn_index from the log (max(turn) + 1)"
    );

    // The first post-resume snapshot must not reuse an existing turn number.
    save_checkpoint(&rt_b, "resumed turn").await;
    let turns = log_turns(&log_path);
    assert_eq!(
        turns,
        vec![0, 1, 2, 3],
        "resumed record continues numbering"
    );
    let mut unique = turns.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique.len(),
        turns.len(),
        "turn numbers must be unique: {turns:?}"
    );

    // Every record carries the canonical `turn_id`, and the post-resume one
    // continues past the pre-resume max instead of restarting (issue #173).
    let recs = recursive::read_checkpoint_log(&log_path).unwrap();
    let pre_max = recs[..3]
        .iter()
        .filter_map(|r| r.turn_id)
        .max()
        .expect("pre-resume records carry turn_id");
    assert_eq!(pre_max, 2, "pre-resume turn_id runs 0..=2");
    assert!(
        recs[3].turn_id.unwrap() > pre_max,
        "post-resume turn_id must continue past the pre-resume max"
    );
    assert_eq!(
        recs[3].turn_id,
        Some(3),
        "post-resume record carries the seeded turn_id"
    );
}

/// Issue #173: `rewind --to-turn N` after a resume must include the
/// post-resume changes in the restored file set.
///
/// Pre-fix the post-resume record reuses an old `turn` (< cutoff), so it is
/// invisible to `plan_rewind`: `z.rs` is left on disk while everything else
/// is restored — code and history silently diverge.
#[tokio::test]
async fn rewind_after_resume_restores_expected_files() {
    if !has_git() {
        return;
    }
    let _home = HomeOverride::new();
    let dir = tempfile::tempdir().unwrap();
    let sw = SessionWriter::create(dir.path(), "e2e-rewind-resume", "mock", "mock").unwrap();
    let session_dir = sw.session_dir().to_path_buf();
    let log_path = session_dir.join("checkpoints.jsonl");
    let session_id = sw.session_id().to_string();
    let shadow = Arc::new(ShadowRepo::open(dir.path()).unwrap());

    // Pre-resume: records 0/1/2. `turn_index` counts completed runs, so a
    // record written *during* turn N carries N; the session ends after the
    // third run, leaving the counter at 3 = max(turn) + 1.
    std::fs::write(dir.path().join("a.txt"), "a-v0").unwrap();
    let (mut rt_a, _touched_a) = wired_runtime(
        dir.path(),
        &shadow,
        &session_id,
        &log_path,
        vec![
            write_file_call("r1", "b.txt", "b-v1"),
            final_completion("t1"),
            write_file_call("r2", "a.txt", "a-v2"),
            final_completion("t2"),
            final_completion("t3"),
        ],
    );
    save_checkpoint(&rt_a, "turn 0").await;
    rt_a.run("write b.txt").await.unwrap();
    save_checkpoint(&rt_a, "turn 1").await;
    rt_a.run("overwrite a.txt").await.unwrap();
    save_checkpoint(&rt_a, "turn 2").await;
    rt_a.run("anything else?").await.unwrap();
    drop(rt_a);
    assert_eq!(log_turns(&log_path), vec![0, 1, 2]);

    // ── resume: new runtime over the same session, continue with z.rs.
    let (mut rt_b, _touched_b) = wired_runtime(
        dir.path(),
        &shadow,
        &session_id,
        &log_path,
        vec![
            write_file_call("r3", "z.rs", "z-resumed"),
            final_completion("t4"),
        ],
    );
    rt_b.run("write z.rs").await.unwrap();
    save_checkpoint(&rt_b, "after z.rs").await;
    drop(rt_b);

    // Rewind to turn 2 (the issue's `sessions rewind --to-turn 2`) → turns 2
    // and 4 are rewound away, so the target is the turn-1 snapshot
    // (a.txt=a-v0, b.txt=b-v1) and z.rs must be dropped.
    //
    // Pre-fix the post-resume record carries turn 1, so it is invisible to
    // this filter and z.rs is silently left behind while a.txt is still
    // restored — code and history diverge.
    let plan = plan_rewind(&log_path, 2).unwrap();
    assert!(
        plan.touched_paths.iter().any(|p| p == "z.rs"),
        "z.rs was touched by a rewound-away turn and must be restored; got {:?}",
        plan.touched_paths
    );
    assert_eq!(plan.turns_to_drop, vec![2, 4]);
    assert_eq!(
        log_turns(&log_path),
        vec![0, 1, 2, 4],
        "post-resume record continues past the pre-resume max"
    );

    apply_rewind(&shadow, &log_path, &plan, false).expect("apply rewind");
    assert!(
        !dir.path().join("z.rs").exists(),
        "post-resume z.rs must be removed"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "a-v0"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("b.txt")).unwrap(),
        "b-v1"
    );
    assert_eq!(log_turns(&log_path), vec![0, 1], "log truncated to < 2");
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
    // Diagnostic: verify snapshot_for_session works before enabling
    // checkpoints.  If this panics the CI log will show the actual
    // git error rather than a silent "0 checkpoints" failure.
    shadow
        .snapshot_for_session(sw.session_id(), "diagnostic-pre-enable")
        .unwrap_or_else(|e| {
            panic!(
                "snapshot_for_session diagnostic failed (workspace={}, session={}):\n{e}",
                dir.path().display(),
                sw.session_id()
            )
        });
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
    // Goal 284: explicitly save a checkpoint after the turn.
    {
        let tools = runtime.kernel().tools();
        let save = tools.get("checkpoint_save").expect("checkpoint_save tool");
        save.execute(json!({"message": "turn 0"})).await.unwrap();
    }
    for m in runtime.transcript().iter().skip(prev) {
        sw.append(m, None, None).unwrap();
    }
    prev = runtime.transcript().len();
    let _o1 = runtime.run("please write b.txt").await.unwrap();
    // Goal 284: save checkpoint after turn 1.
    {
        let tools = runtime.kernel().tools();
        let save = tools.get("checkpoint_save").expect("checkpoint_save tool");
        save.execute(json!({"message": "turn 1"})).await.unwrap();
    }
    for m in runtime.transcript().iter().skip(prev) {
        sw.append(m, None, None).unwrap();
    }
    sw.finish(SessionStatus::Completed).unwrap();

    // Both files should exist on disk now.
    let a_path = dir.path().join("a.txt");
    let b_path = dir.path().join("b.txt");
    assert_eq!(std::fs::read_to_string(&a_path).unwrap(), "v-turn-0");
    assert_eq!(std::fs::read_to_string(&b_path).unwrap(), "v-turn-1");

    // Goal 284: checkpoints saved after run() have turns 1 and 2
    // (turn_index is incremented at end of run()).
    let recs = recursive::read_checkpoint_log(&log_path).unwrap();
    assert_eq!(recs.len(), 2);
    assert_eq!(recs[0].turn, 1);
    assert_eq!(recs[1].turn, 2);
    assert!(recs[0].touched_files.iter().any(|p| p == "a.txt"));
    assert!(recs[1].touched_files.iter().any(|p| p == "b.txt"));

    // ── Rewind to turn 2 (undo turn 2's changes) ────────────────────
    let plan = plan_rewind(&log_path, 2).unwrap();
    let result = apply_rewind(&shadow, &log_path, &plan, false).expect("apply rewind");
    assert_eq!(result.dropped_turns, vec![2]);

    // a.txt unchanged; b.txt deleted (it didn't exist at the turn-1 checkpoint).
    assert_eq!(std::fs::read_to_string(&a_path).unwrap(), "v-turn-0");
    assert!(!b_path.exists(), "b.txt should be gone after rewind");

    // checkpoints.jsonl now only has turn 1.
    let recs = recursive::read_checkpoint_log(&log_path).unwrap();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].turn, 1);

    // Truncate transcript.jsonl to turn 1 (drops turn 1's user message
    // and everything after; keeps turn 0's messages).
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
    // Diagnostic: verify snapshot_for_session works before enabling checkpoints.
    shadow
        .snapshot_for_session(sw.session_id(), "diagnostic-pre-enable")
        .unwrap_or_else(|e| {
            panic!(
                "snapshot_for_session diagnostic failed (workspace={}, session={}):\n{e}",
                dir.path().display(),
                sw.session_id()
            )
        });
    runtime
        .enable_checkpoints(
            Arc::clone(&shadow),
            sw.session_id().to_string(),
            log_path.clone(),
            Some(touched),
        )
        .unwrap();

    // Save an initial checkpoint before any turn (Goal 284: the agent
    // should checkpoint before starting risky work).
    {
        let tools = runtime.kernel().tools();
        let save = tools.get("checkpoint_save").expect("checkpoint_save tool");
        save.execute(json!({"message": "initial"})).await.unwrap();
    }

    runtime.run("write agent.txt").await.unwrap();
    // Goal 284: explicitly save a checkpoint after the turn.
    {
        let tools = runtime.kernel().tools();
        let save = tools.get("checkpoint_save").expect("checkpoint_save tool");
        save.execute(json!({"message": "after write"}))
            .await
            .unwrap();
    }

    // Simulate an external editor modifying untouched.txt after the
    // turn finished.
    std::fs::write(&untouched, "external-edit").unwrap();

    // Rewind to turn 0 (the initial checkpoint): agent.txt should be
    // deleted (it didn't exist), untouched.txt should NOT be reverted.
    let plan = plan_rewind(&log_path, 0).unwrap();
    apply_rewind(&shadow, &log_path, &plan, false).unwrap();
    assert!(!dir.path().join("agent.txt").exists());
    assert_eq!(
        std::fs::read_to_string(&untouched).unwrap(),
        "external-edit",
        "untouched.txt must not be reverted by rewind"
    );
}
