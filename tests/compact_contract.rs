//! Goal 411 — context & compaction contract (1/2): one owner, one path.
//!
//! `AgentRuntime` owns the transcript; `Compactor::apply` is the only API
//! allowed to rewrite it. These tests are the structural contract plus its
//! red-light negative controls — each control names the mutation it catches.
//!
//! Scope note: goal 413 owns canonical rebuild / token normalisation /
//! degradation observability and is deliberately absent here.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use recursive::compact::verify_compaction_rewrite;
use recursive::llm::{Completion, MockProvider};
use recursive::message::{Message, Role};
use recursive::{AgentRuntime, Compactor};

fn summary_completion(text: &str) -> Completion {
    Completion {
        content: text.to_string(),
        tool_calls: vec![],
        finish_reason: Some("stop".to_string()),
        usage: None,
        reasoning_content: None,
    }
}

/// Collect every production `src/**/*.rs` outside `src/compact/` that names
/// the split computation. Goal 411's single path means exactly one module may
/// know where a transcript is cut. Test modules are skipped: a negative
/// control is allowed to name the mechanism it mutates.
fn scan_for_split_calls(dir: &Path, src_root: &Path, offenders: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir)
        .expect("src must be listable")
        .flatten()
    {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            if path == src_root.join("compact") || name == "tests" {
                continue;
            }
            scan_for_split_calls(&path, src_root, offenders);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") || name == "tests.rs" {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("source file must be readable");
        for needle in ["safe_split_point", "leading_system_prefix_len"] {
            if text.contains(needle) {
                offenders.push(format!("{} references {needle}", path.display()));
            }
        }
    }
}

/// Headline contract: static "no split computation outside `src/compact/`"
/// plus the behavioural shape a runtime compaction must leave behind.
#[tokio::test]
async fn test_compact_contract_single_path() {
    // ── Static half ───────────────────────────────────────────────────────
    let src_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    scan_for_split_calls(&src_root, &src_root, &mut offenders);
    assert!(
        offenders.is_empty(),
        "only src/compact/ may compute a compaction split; offenders: {offenders:#?}"
    );

    // ── Behavioural half: runtime rewrite == Compactor::apply's rewrite ───
    let before = vec![
        Message::system("sys"),
        Message::user("u0"),
        Message::assistant("a0"),
        Message::user("u1"),
        Message::assistant("a1"),
        Message::user("u2"),
        Message::assistant("a2"),
    ];
    let compactor = Compactor::new(1).keep_recent_n(2);

    // Direct application: the returned bounds describe the splice exactly.
    let provider = MockProvider::new(vec![summary_completion("summary")]);
    let mut applied = before.clone();
    let outcome = compactor
        .apply(&provider, &mut applied, 0)
        .await
        .unwrap()
        .expect("transcript is long enough to compact");
    assert_eq!(outcome.bounds.prefix, 0..1);
    assert_eq!(outcome.bounds.summarized, 1..5);
    assert_eq!(outcome.bounds.preserved, 5..7);
    assert_eq!(outcome.removed, 4);
    assert_eq!(&applied[..1], &before[..1], "prefix verbatim");
    assert_eq!(applied[1], outcome.summary, "summary at prefix end");
    assert_eq!(&applied[2..], &before[5..], "preserved tail verbatim");
    assert!(verify_compaction_rewrite(&before, &applied, &outcome));

    // Runtime cross-turn rewrite must produce the very same shape — its
    // rewrite is `Compactor::apply`, not a parallel splice.
    let mut rt = AgentRuntime::builder()
        .llm(Arc::new(MockProvider::new(vec![summary_completion(
            "summary",
        )])))
        .compactor(Compactor::new(1).keep_recent_n(2))
        .build()
        .unwrap();
    rt.set_transcript(before.clone());
    rt.compact_now().await.unwrap();
    assert_eq!(rt.transcript(), applied.as_slice());
}

/// Negative control — mutation: drop the leading-System prefix boundary
/// (`bounds.summarized.start = prefix.end`; i.e. summarise from index 0).
///
/// With the protection removed, the rebuilt prompt is folded into the
/// summary: the head of the transcript changes and `bounds.summarized`
/// starts at 0 — both assertions below turn red.
#[tokio::test]
async fn test_compact_contract_negative_control_prefix_protection() {
    let before = vec![
        Message::system("prompt-a"),
        Message::system("prompt-b"),
        Message::user("u0"),
        Message::assistant("a0"),
        Message::user("u1"),
        Message::assistant("a1"),
    ];
    let provider = MockProvider::new(vec![summary_completion("summary")]);
    let compactor = Compactor::new(1).keep_recent_n(2);
    let mut after = before.clone();

    let outcome = compactor
        .apply(&provider, &mut after, 0)
        .await
        .unwrap()
        .expect("two conversational messages in the older slice");

    assert_eq!(
        outcome.bounds.prefix,
        0..2,
        "both rebuilt System prompts are the prefix"
    );
    assert_eq!(
        outcome.bounds.summarized.start, outcome.bounds.prefix.end,
        "summarisation starts after the prefix, never at index 0"
    );
    assert_eq!(outcome.removed, outcome.bounds.summarized.len());
    assert_eq!(
        &after[..2],
        &before[..2],
        "the System prefix must survive verbatim"
    );
    assert_eq!(after[2], outcome.summary);
}

/// Negative control — mutation: delete the Tool / Assistant-with-`tool_calls`
/// back-up in `safe_split_point`.
///
/// The naive split (`len - keep_n`) lands on a `Tool` result here; without
/// the back-up the preserved region starts with an orphan tool result, so
/// the "preserved tail starts at a User message" assertions turn red.
#[tokio::test]
async fn test_compact_contract_negative_control_tool_call_pairing() {
    use recursive::message::ToolCall;

    let call = ToolCall {
        id: "c1".into(),
        name: "Read".into(),
        arguments: serde_json::json!({}),
    };
    // len 8, keep_n 3 → naive split 5 = the Tool result.
    let before = vec![
        Message::system("sys"),
        Message::user("u0"),
        Message::assistant("a0"),
        Message::user("u1"),
        Message::assistant_with_tool_calls("a1", vec![call]),
        Message::tool_result("call_c1", "file contents"),
        Message::user("u2"),
        Message::assistant("a2"),
    ];
    let provider = MockProvider::new(vec![summary_completion("summary")]);
    let compactor = Compactor::new(1).keep_recent_n(3);
    let mut after = before.clone();

    let outcome = compactor
        .apply(&provider, &mut after, 0)
        .await
        .unwrap()
        .expect("the backed-up older slice has two conversational messages");

    let kept_start = outcome.bounds.preserved.start;
    assert_eq!(
        kept_start, 3,
        "back-up must land the split on u1, not on the Tool result at index 5"
    );
    assert_eq!(
        before[kept_start].role,
        Role::User,
        "preserved tail starts at a User message, never an orphan Tool result"
    );
    assert_eq!(
        &after[outcome.bounds.summarized.start + 1..],
        &before[kept_start..],
        "summary at prefix end, preserved tail verbatim from u1 onwards"
    );
}

/// Negative control — mutation: a second rewrite path that edits the
/// transcript directly instead of going through `Compactor::apply`.
///
/// A hand-rolled drain/insert can produce a plausible-looking transcript but
/// it is not the recorded compaction, so `verify_compaction_rewrite` rejects
/// it. If a caller ever swaps `apply` for a bespoke splice, this control
/// (and the runtime `debug_assert!` that uses the same predicate) goes red.
#[tokio::test]
async fn test_compact_contract_negative_control_bypassing_apply() {
    let before = vec![
        Message::system("sys"),
        Message::user("u0"),
        Message::assistant("a0"),
        Message::user("u1"),
        Message::assistant("a1"),
        Message::user("u2"),
        Message::assistant("a2"),
    ];
    let provider = MockProvider::new(vec![summary_completion("summary")]);
    let compactor = Compactor::new(1).keep_recent_n(2);
    let mut via_apply = before.clone();
    let outcome = compactor
        .apply(&provider, &mut via_apply, 0)
        .await
        .unwrap()
        .expect("transcript is long enough to compact");
    assert!(verify_compaction_rewrite(&before, &via_apply, &outcome));

    // Second path #1: same cut, hand-written summary message.
    let mut bypass_summary = before.clone();
    bypass_summary.drain(outcome.bounds.summarized.clone());
    bypass_summary.insert(
        outcome.bounds.summarized.start,
        Message::system("[compacted: hand-written]").with_compaction_summary(),
    );
    assert!(
        !verify_compaction_rewrite(&before, &bypass_summary, &outcome),
        "a hand-written summary must not pass as the recorded compaction"
    );

    // Second path #2: a different cut that also drops pairing context.
    let mut bypass_cut = before.clone();
    bypass_cut.drain(1..4);
    bypass_cut.insert(outcome.bounds.summarized.start, outcome.summary.clone());
    assert!(
        !verify_compaction_rewrite(&before, &bypass_cut, &outcome),
        "a different drain range must not pass as the recorded compaction"
    );
}
