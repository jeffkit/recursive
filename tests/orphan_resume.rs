//! Integration tests for Goal 153 — orphan tool-call detection on resume.
//!
//! `cmd_resume` consumes [`SessionReader::scan_orphan_tool_calls`] and routes
//! each orphan through one of three policies (`ask`, `skip`, `redo`). The
//! interactive `ask` path lives in `main.rs` behind stdin and TTY checks, so
//! it cannot be exercised from a library test directly. Instead these tests
//! lock down the behaviour that policies *depend* on, asserting that
//! `scan_orphan_tool_calls` produces inputs the three branches can act on
//! correctly:
//!
//! * `skip`  — needs the orphan list at all (any orphan triggers the branch);
//! * `redo`  — needs `side_effect_at_call` so it can warn before re-running
//!   `External` tools;
//! * `ask`   — needs `tool_name` + `args_hash` for the per-orphan prompt.
//!
//! These three concerns are covered by the cases below. The unit-level
//! `resume_after_crash_orphan_visible` in `tests/incremental_writes.rs`
//! verifies that the orphan *shape* survives reload; this file verifies the
//! orphan *metadata* the policy branches read.

use std::sync::Arc;

use recursive::llm::ToolSpec;
use recursive::message::{Message, Role};
use recursive::session::{OrphanToolCall, SessionReader, SessionWriter};
use recursive::test_util::PinnedRecursiveHome;
use recursive::tools::{Tool, ToolRegistry, ToolSideEffect};
use serde_json::json;

// ── Stub tools (just enough to populate ToolRegistry with side_effect_class) ──

struct StubTool {
    name: &'static str,
    side_effect: ToolSideEffect,
}

#[async_trait::async_trait]
impl Tool for StubTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.into(),
            description: "stub".into(),
            parameters: json!({"type": "object"}),
        }
    }

    async fn execute(&self, _args: serde_json::Value) -> recursive::Result<String> {
        Ok(String::new())
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        self.side_effect
    }
}

fn registry_with(tools: &[(&'static str, ToolSideEffect)]) -> ToolRegistry {
    let mut reg = ToolRegistry::local();
    for (name, side_effect) in tools {
        reg.register_mut(Arc::new(StubTool {
            name,
            side_effect: *side_effect,
        }));
    }
    reg
}

struct HomePin {
    _dir: tempfile::TempDir,
    _pin: PinnedRecursiveHome,
}

impl HomePin {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let pin = PinnedRecursiveHome::new(dir.path());
        Self {
            _dir: dir,
            _pin: pin,
        }
    }
}

fn write_orphan_session(
    workspace: &std::path::Path,
    calls: &[(&str, &str, serde_json::Value)],
) -> std::path::PathBuf {
    let mut w =
        SessionWriter::create(workspace, "orphan test", "mock-model", "mock").expect("create");

    w.append(&Message::user("kick off"), None, None)
        .expect("user");

    let tool_calls: Vec<recursive::llm::ToolCall> = calls
        .iter()
        .map(|(id, name, args)| recursive::llm::ToolCall {
            id: (*id).into(),
            name: (*name).into(),
            arguments: args.clone(),
        })
        .collect();

    w.append(
        &Message {
            role: Role::Assistant,
            content: "calling tools".into(),
            tool_calls,
            tool_call_id: None,
            reasoning_content: None,
        },
        None,
        None,
    )
    .expect("assistant w/ tool_calls");

    // ──── crash here — no tool result message is appended ────

    let dir = w.session_dir().to_path_buf();
    drop(w); // release SessionLock so SessionReader can re-open
    dir
}

// ── Test 1: `skip` policy — any orphan must surface in the scan list ──────

#[test]
fn skip_policy_sees_every_unanswered_call() {
    let _home = HomePin::new();
    let ws = std::path::PathBuf::from("/tmp/g153-skip-test-ws");
    let dir = write_orphan_session(
        &ws,
        &[
            ("tc_a", "run_shell", json!({"cmd": "ls"})),
            ("tc_b", "run_shell", json!({"cmd": "pwd"})),
        ],
    );

    let registry = registry_with(&[("run_shell", ToolSideEffect::External)]);
    let orphans: Vec<OrphanToolCall> =
        SessionReader::scan_orphan_tool_calls(&dir, &registry).expect("scan");

    assert_eq!(orphans.len(), 2, "both unanswered calls must be reported");
    let ids: std::collections::HashSet<_> =
        orphans.iter().map(|o| o.tool_call_id.clone()).collect();
    assert!(ids.contains("tc_a"));
    assert!(ids.contains("tc_b"));
}

// ── Test 2: `redo` policy — side_effect_at_call must reflect the registry ──

#[test]
fn redo_policy_gets_side_effect_for_warning() {
    let _home = HomePin::new();
    let ws = std::path::PathBuf::from("/tmp/g153-redo-test-ws");
    let dir = write_orphan_session(
        &ws,
        &[
            ("tc_safe", "estimate_tokens", json!({"text": "hi"})),
            ("tc_risky", "run_shell", json!({"cmd": "curl bad.example"})),
        ],
    );

    let registry = registry_with(&[
        ("estimate_tokens", ToolSideEffect::ReadOnly),
        ("run_shell", ToolSideEffect::External),
    ]);
    let orphans = SessionReader::scan_orphan_tool_calls(&dir, &registry).expect("scan");

    let by_id: std::collections::HashMap<_, _> = orphans
        .iter()
        .map(|o| (o.tool_call_id.clone(), o))
        .collect();
    assert!(matches!(
        by_id["tc_safe"].side_effect_at_call,
        ToolSideEffect::ReadOnly
    ));
    // The `redo` branch keys off `External` to print the safety warning;
    // any drift here would silence the warning.
    assert!(matches!(
        by_id["tc_risky"].side_effect_at_call,
        ToolSideEffect::External
    ));
}

// ── Test 3: `ask` policy — args_hash + tool_name needed for the prompt ────

#[test]
fn ask_policy_gets_tool_name_and_args_hash() {
    let _home = HomePin::new();
    let ws = std::path::PathBuf::from("/tmp/g153-ask-test-ws");
    let args = json!({"cmd": "echo hello"});
    let dir = write_orphan_session(&ws, &[("tc_only", "run_shell", args.clone())]);

    let registry = registry_with(&[("run_shell", ToolSideEffect::External)]);
    let orphans = SessionReader::scan_orphan_tool_calls(&dir, &registry).expect("scan");

    assert_eq!(orphans.len(), 1);
    let o = &orphans[0];
    assert_eq!(o.tool_name, "run_shell");
    assert_eq!(o.tool_call_id, "tc_only");

    // args_hash must match the BLAKE3 of `arguments.to_string()` so the
    // resume path can detect drift between recorded args and what gets
    // re-executed.
    let expected = blake3::hash(args.to_string().as_bytes())
        .to_hex()
        .to_string();
    assert_eq!(o.args_hash, expected);
}

// ── Test 4: missing tool in registry falls back to External ───────────────

#[test]
fn unknown_tool_falls_back_to_external() {
    let _home = HomePin::new();
    let ws = std::path::PathBuf::from("/tmp/g153-unknown-tool-ws");
    let dir = write_orphan_session(&ws, &[("tc_x", "tool_that_no_longer_exists", json!({}))]);

    let registry = registry_with(&[]); // no tools registered
    let orphans = SessionReader::scan_orphan_tool_calls(&dir, &registry).expect("scan");

    assert_eq!(orphans.len(), 1);
    // `cmd_resume` validates the registry hash before this point in
    // production. A test registry with no tools simulates a registry that
    // cannot classify the call — fall back must be `External` so `redo`
    // shows the conservative warning.
    assert!(matches!(
        orphans[0].side_effect_at_call,
        ToolSideEffect::External
    ));
}

// ── Test 5: clean transcript — no orphan = empty result ───────────────────

#[test]
fn answered_calls_are_not_orphans() {
    let _home = HomePin::new();
    let ws = std::path::PathBuf::from("/tmp/g153-clean-ws");

    let mut w = SessionWriter::create(&ws, "clean", "mock-model", "mock").expect("create");
    w.append(&Message::user("hi"), None, None).unwrap();
    w.append(
        &Message {
            role: Role::Assistant,
            content: "ok".into(),
            tool_calls: vec![recursive::llm::ToolCall {
                id: "tc_done".into(),
                name: "run_shell".into(),
                arguments: json!({"cmd": "ls"}),
            }],
            tool_call_id: None,
            reasoning_content: None,
        },
        None,
        None,
    )
    .unwrap();
    w.append(
        &Message {
            role: Role::Tool,
            content: "tool returned ok".into(),
            tool_calls: vec![],
            tool_call_id: Some("tc_done".into()),
            reasoning_content: None,
        },
        None,
        None,
    )
    .unwrap();
    let dir = w.session_dir().to_path_buf();
    drop(w);

    let registry = registry_with(&[("run_shell", ToolSideEffect::External)]);
    let orphans = SessionReader::scan_orphan_tool_calls(&dir, &registry).expect("scan");
    assert!(
        orphans.is_empty(),
        "answered tool calls must not be orphaned, got {orphans:?}"
    );
}
