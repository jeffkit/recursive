//! Goal #133 acceptance tests: declared deliverables + per-turn change ledger.
//!
//! These drive the public surface only (registry + runtime + events) and
//! assert the three things the goal asks for:
//!
//! 1. a turn's file changes produce a renderable ledger (added / modified /
//!    deleted + diffs) while the user's repository stays untouched — no index,
//!    HEAD or status change, and pre-existing uncommitted edits are not
//!    attributed to the agent's turn;
//! 2. `Present` surfaces paths and an event, and the file bodies never reach
//!    the transcript;
//! 3. over-budget paths degrade explicitly (`coarse`) instead of failing.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};

use recursive::deliverables::{Budgets, ChangeStatus, Deliverables};
use recursive::event::{AgentEvent, ChannelSink, EventSink};
use recursive::llm::{Completion, MockProvider, ToolCall};
use recursive::runtime::AgentRuntime;
use recursive::tools::build_standard_tools;
use serde_json::json;
use tempfile::TempDir;

/// `RECURSIVE_HOME` decides where per-workspace state (including the ledger's
/// blob store and shadow index) lands. Point it at one fixed temp dir for the
/// whole test binary before any test runs — a single value, so the
/// process-global env cannot race between threads.
fn isolate_home() {
    static HOME: OnceLock<()> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir =
            std::env::temp_dir().join(format!("recursive-deliverables-it-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::env::set_var("RECURSIVE_HOME", dir);
    });
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git is required for these tests");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn repo_workspace() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    git(&ws, &["init", "-q"]);
    git(&ws, &["config", "user.email", "t@example.com"]);
    git(&ws, &["config", "user.name", "t"]);
    std::fs::write(ws.join("README.md"), "hello\n").unwrap();
    git(&ws, &["add", "--all"]);
    git(&ws, &["commit", "-q", "-m", "init", "--no-gpg-sign"]);
    (tmp, ws)
}

/// Script one or more tool-calling steps plus the final text answer (the
/// mock provider errors once its queue runs dry, so the answer is required).
fn scripted(steps: Vec<Vec<ToolCall>>, final_text: &str) -> Arc<MockProvider> {
    let mut script: Vec<Completion> = steps
        .into_iter()
        .map(|calls| Completion {
            content: String::new(),
            tool_calls: calls,
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        })
        .collect();
    script.push(Completion {
        content: final_text.to_string(),
        tool_calls: Vec::new(),
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    });
    Arc::new(MockProvider::new(script))
}

fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

#[test]
fn registry_offers_the_deliverables_surface() {
    isolate_home();
    let (_tmp, ws) = repo_workspace();
    let registry = build_standard_tools(&ws, &[], 30);
    assert!(registry.find_by_name("Present").is_some());
    assert!(registry.find_by_name("ChangeLedger").is_some());
    let ledger = registry
        .deliverables()
        .expect("ledger wired into the registry");
    assert_eq!(ledger.workspace(), ws);
}

#[tokio::test]
async fn present_surfaces_paths_and_emits_an_event_without_bodies() {
    isolate_home();
    let (_tmp, ws) = repo_workspace();
    std::fs::create_dir_all(ws.join("out")).unwrap();
    std::fs::write(ws.join("out/report.md"), "TOP-SECRET-BODY").unwrap();

    let tools = build_standard_tools(&ws, &[], 30);
    let ledger = tools.deliverables().unwrap();
    ledger.begin_turn(1);

    let out = tools
        .invoke("Present", json!({"files": ["out/report.md"]}))
        .await
        .unwrap();
    assert!(out.contains("out/report.md"), "{out}");
    assert!(
        !out.contains("TOP-SECRET-BODY"),
        "file bytes must never reach the transcript: {out}"
    );

    let changes = ledger.finalize_turn(1).unwrap();
    assert_eq!(changes.presented.len(), 1);
    assert_eq!(changes.presented[0].path, "out/report.md");
    assert!(!changes.render().contains("TOP-SECRET-BODY"));
}

#[tokio::test]
async fn a_turn_produces_a_renderable_ledger_and_leaves_git_untouched() {
    isolate_home();
    let (_tmp, ws) = repo_workspace();
    // A pre-existing, uncommitted user edit — must survive the turn untouched.
    std::fs::write(ws.join("README.md"), "hello\nuser-wip\n").unwrap();
    let index_before = std::fs::read(ws.join(".git/index")).unwrap();
    let head_before = std::fs::read(ws.join(".git/HEAD")).unwrap();
    let status_before = git(&ws, &["status", "--porcelain"]);

    let tools = build_standard_tools(&ws, &[], 30);
    let (sink, mut rx) = ChannelSink::new();
    let sink: Arc<dyn EventSink> = Arc::new(sink);
    let llm = scripted(
        vec![vec![
            ToolCall {
                id: "c1".into(),
                name: "Write".into(),
                arguments: json!({"path": "src/new.rs", "contents": "fn main() {}\n"}),
            },
            ToolCall {
                id: "c2".into(),
                name: "Present".into(),
                arguments: json!({"files": ["src/new.rs"]}),
            },
        ]],
        "wrote and declared the deliverable",
    );

    let mut runtime = AgentRuntime::builder()
        .llm(llm)
        .tools(tools)
        .event_sink(sink)
        .max_steps(6)
        .build()
        .unwrap();
    runtime.run("add src/new.rs").await.unwrap();

    let events = drain(&mut rx);
    let ledger_event = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ChangeLedger { turn, changes } if changes.turn == *turn => Some(changes),
            _ => None,
        })
        .expect("a ChangeLedger event for the turn");
    assert_eq!(ledger_event.added.len(), 1, "{ledger_event:?}");
    assert_eq!(ledger_event.added[0].path, "src/new.rs");
    assert_eq!(ledger_event.added[0].status, ChangeStatus::Added);
    assert!(
        ledger_event.added[0].diff.contains("+fn main() {}"),
        "{}",
        ledger_event.added[0].diff
    );
    assert_eq!(ledger_event.presented.len(), 1);
    assert_eq!(ledger_event.presented[0].path, "src/new.rs");
    assert!(
        events.iter().any(
            |e| matches!(e, AgentEvent::DeliverablesPresented { files, .. } if files.len() == 1)
        ),
        "a deliverables/presented event must accompany a successful Present"
    );

    // Ordering contract: the ledger closes the turn BEFORE the turn boundary,
    // so a consumer reacting to `TurnFinished` already holds the ledger.
    let ledger_pos = events
        .iter()
        .position(|e| matches!(e, AgentEvent::ChangeLedger { .. }))
        .expect("a ChangeLedger event");
    let finished_pos = events
        .iter()
        .position(|e| matches!(e, AgentEvent::TurnFinished { .. }))
        .expect("a TurnFinished event");
    assert!(
        ledger_pos < finished_pos,
        "ChangeLedger must be emitted before TurnFinished"
    );

    // The user's repository is untouched: same index, same HEAD, and the
    // agent's new file is simply untracked on top of the user's own edit.
    assert_eq!(std::fs::read(ws.join(".git/index")).unwrap(), index_before);
    assert_eq!(std::fs::read(ws.join(".git/HEAD")).unwrap(), head_before);
    let status_after = git(&ws, &["status", "--porcelain"]);
    assert_eq!(
        status_after,
        format!("{status_before}?? src/\n"),
        "the agent's own file is the only working-tree delta"
    );
    assert!(!ws.join(".git/index.lock").exists());
}

#[tokio::test]
async fn uncommitted_user_edits_are_not_absorbed_into_the_turn() {
    isolate_home();
    let (_tmp, ws) = repo_workspace();
    std::fs::write(ws.join("README.md"), "hello\nuser-wip\n").unwrap();

    let tools = build_standard_tools(&ws, &[], 30);
    let ledger = tools.deliverables().unwrap();
    ledger.begin_turn(0);
    ledger.ensure_baseline().unwrap();
    std::fs::write(ws.join("README.md"), "hello\nuser-wip\nagent-touch\n").unwrap();
    let changes = ledger.finalize_turn(0).unwrap();

    assert_eq!(changes.modified.len(), 1);
    let change = &changes.modified[0];
    // The diff is against the *baseline* (the user's uncommitted version),
    // not against HEAD — so only the agent's own line shows up.
    assert!(change.diff.contains("+agent-touch\n"), "{}", change.diff);
    assert!(!change.diff.contains("+user-wip"), "{}", change.diff);
}

#[test]
fn over_budget_paths_degrade_with_a_coarse_marker() {
    isolate_home();
    let (_tmp, ws) = repo_workspace();
    let budgets = Budgets {
        max_files: 2,
        max_file_bytes: 8,
        ..Budgets::default()
    };
    let ledger = Deliverables::new(
        &ws,
        std::env::temp_dir().join(format!(
            "recursive-deliverables-budgeted-{}",
            std::process::id()
        )),
        budgets,
    )
    .unwrap();

    ledger.begin_turn(1);
    ledger.ensure_baseline().unwrap();
    // Sorts first, so it keeps its own (capture-budget) reason instead of
    // being overwritten by the render budget the way later paths are.
    std::fs::write(ws.join("aa-big.txt"), "0123456789").unwrap();
    for i in 0..4 {
        std::fs::write(ws.join(format!("f{i}.txt")), "tiny\n").unwrap();
    }
    let changes = ledger.finalize_turn(1).unwrap();

    assert_eq!(changes.changed_count(), 5);
    assert!(changes.truncated, "the render budget must be reported");
    assert!(changes.coarse_count() >= 3, "{changes:?}");
    let oversized = changes
        .all_changes()
        .find(|c| c.path == "aa-big.txt")
        .expect("the over-budget file is still listed");
    assert!(oversized.coarse);
    assert!(
        oversized
            .coarse_reason
            .clone()
            .unwrap_or_default()
            .contains("capture budget"),
        "{oversized:?}"
    );
    let rendered = changes.render();
    assert!(rendered.contains("coarse"), "{rendered}");
}
