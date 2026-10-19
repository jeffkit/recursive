//! Goal 411 — context & compaction contract (1/2): one owner, one path.
//! Goal 413 — context & compaction contract (2/2): the persisted boundary and
//! the canonical rebuild.
//!
//! `AgentRuntime` owns the transcript; `Compactor::apply` is the only API
//! allowed to rewrite it, and the marker row `apply` splices in is the only
//! thing a persisted transcript may rebuild from. These tests are the
//! structural contract plus red-light negative controls — each control names
//! the mutation it catches.
//!
//! Scope note: token normalisation / degradation observability belong to
//! goal 414 and are deliberately absent here.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use recursive::compact::rebuild_declared_seed;
use recursive::compact::rebuild_seed;
use recursive::compact::verify_compaction_rewrite;
use recursive::llm::{Completion, MockProvider};
use recursive::message::{Message, Role};
use recursive::session::{SessionPersistenceSink, SessionReader, SessionStatus, SessionWriter};
use recursive::test_util::IsolatedWorkspace;
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

// ═══════════════════════════════════════════════════════════════════════════
// Goal 413 — canonical rebuild from the persisted boundary
// ═══════════════════════════════════════════════════════════════════════════
//
// Goal 411 made `Compactor::apply` the single *write* path; goal 413 makes
// the marker row it splices in the single *rebuild* boundary. The mechanism
// chosen for boundary persistence (see src/compact/rebuild.rs): the summary
// row's `is_compaction_summary` bit itself, round-tripped by every writer
// (`TranscriptEntry` persists it, so the JSONL session log, the AG-UI mirror
// and the legacy-transcript rewrite all carry it), with the g157
// `compact_boundary` line as the same boundary for transcripts written
// before the field existed. A separate checkpoint marker document was
// rejected: it would need migration and a second source of truth for a
// boundary the conversation rows already hold.

/// Append `msgs` through the persistence sink the way a live runtime does,
/// so the JSONL row shape is the production one (`SessionWriter::append`).
async fn persist_through_sink(sw: &Arc<std::sync::Mutex<SessionWriter>>, msgs: &[Message]) {
    use recursive::event::EventSink;
    let sink = SessionPersistenceSink::new(sw.clone());
    for m in msgs {
        sink.emit(recursive::AgentEvent::MessageAppended {
            message: m.clone(),
            usage: None,
            step: None,
        })
        .await;
    }
}

/// Run a real compaction through `Compactor::apply` (the goal-411 single
/// path) and persist the post-compaction transcript through the session
/// sink — the exact rows a resume must rebuild from.
async fn compact_and_persist(ws: &IsolatedWorkspace) -> (std::path::PathBuf, Vec<Message>) {
    let sw = Arc::new(std::sync::Mutex::new(
        SessionWriter::create(ws.path(), "g413", "m", "openai").unwrap(),
    ));
    let dir = sw.lock().unwrap().session_dir().to_path_buf();

    let before = vec![
        Message::system("sys"),
        Message::user("u0"),
        Message::assistant("a0"),
        Message::user("u1"),
        Message::assistant("a1"),
        Message::user("u2"),
        Message::assistant("a2"),
    ];
    persist_through_sink(&sw, &before).await;

    let provider = MockProvider::new(vec![summary_completion("summary of u0..a1")]);
    let compactor = Compactor::new(1).keep_recent_n(2);
    let mut in_memory = before.clone();
    let outcome = compactor
        .apply(&provider, &mut in_memory, 0)
        .await
        .unwrap()
        .expect("transcript is long enough to compact");

    // Persist exactly the post-compaction in-memory state, in production
    // order: `CompactionBoundary` first (the g157 gate line, which makes the
    // reader drop the pre-boundary rows on load), then the summary row (the
    // runtime re-emits the marker message itself, src/runtime.rs), then the
    // preserved tail. Appending the tail before the boundary event would
    // fold it away on load — a fixture bug, not a contract one.
    let sink = SessionPersistenceSink::new(sw.clone());
    use recursive::event::EventSink;
    sink.emit(recursive::AgentEvent::CompactionBoundary {
        turn: 0,
        compacted_count: outcome.removed,
        summary_uuid: None,
        cache_hit_tokens: 0,
        cache_miss_tokens: 0,
        is_recompaction_in_chain: false,
        turns_since_previous_compact: 0,
        previous_compact_turn: None,
    })
    .await;
    for m in in_memory.iter().skip(1) {
        sink.emit(recursive::AgentEvent::MessageAppended {
            message: m.clone(),
            usage: None,
            step: None,
        })
        .await;
    }
    sw.lock().unwrap().finish(SessionStatus::Completed).unwrap();
    (dir, in_memory)
}

/// Headline: the persisted boundary is rebuildable, and every consumer
/// framing (`SessionReader::load_messages`) reconstructs exactly the
/// post-compaction in-memory state — 「重建结果 ≡ 压缩后内存态」.
#[tokio::test]
async fn test_compact_contract_rebuild_markers() {
    let ws = IsolatedWorkspace::new();
    let (dir, in_memory) = compact_and_persist(&ws).await;

    // The persisted transcript carries the marker: the boundary row wrote
    // its `is_compaction_summary` bit, and the reader round-trips it. The
    // g157 `compact_boundary` line the sink also writes is superseded
    // metadata and is skipped by `load_transcript`, so the model-visible
    // window is exactly the post-boundary rows.
    let stored = SessionReader::load_messages(&dir).unwrap();
    let marker_rows = stored.iter().filter(|m| m.is_compaction_summary).count();
    assert_eq!(
        marker_rows, 1,
        "exactly one persisted boundary marker row; got {stored:?}"
    );

    // Canonical rebuild ≡ post-compaction in-memory state. The runtime's
    // own system prompt is not part of a seed (it reassembles it), so the
    // comparison drops the leading prompt from the in-memory side — that is
    // the same framing `rebuild_seed` applies to the stored rows.
    let rebuilt = rebuild_seed(&stored).expect("a compacted session is restorable");
    let expected: Vec<Message> = in_memory
        .iter()
        .filter(|m| !(m.role == Role::System && !m.is_compaction_summary))
        .cloned()
        .collect();
    assert_eq!(
        rebuilt.seed, expected,
        "rebuild from the persisted marker must equal the post-compaction memory"
    );
    assert!(rebuilt.seed[0].is_compaction_summary, "summary leads");

    // And through the consumer entry point itself: `load_messages` IS the
    // canonical rebuild (goal 413), so resume gets the right seed with no
    // second framing.
    let seed = SessionReader::load_messages(&dir).unwrap();
    assert_eq!(seed, expected, "resume seed == post-compaction memory");
}

/// Review BLOCKER: every reader of an append-only file hands the whole file
/// and its last declaration index to [`rebuild_declared_seed`]. The row right
/// after the declaration is the summary; a pre-goal-413 writer persisted it
/// without the marker bit, and dropping it with the stored prompt would erase
/// every folded turn silently (#158's failure class).
#[test]
fn test_compact_contract_rebuild_declared_boundary_backfills_a_legacy_summary() {
    let stored = vec![
        Message::system("stored prompt"),
        Message::user("u1"),
        Message::assistant("a1"),
        Message::system("[compacted: u1 a1]"),
        Message::user("u2"),
        Message::assistant("a2"),
    ];
    let rebuilt = rebuild_declared_seed(&stored, Some(3)).expect("restorable");
    let contents: Vec<&str> = rebuilt.seed.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(
        contents,
        vec!["[compacted: u1 a1]", "u2", "a2"],
        "the unmarked summary must survive and lead the seed"
    );
    assert!(
        rebuilt.seed[0].is_compaction_summary,
        "the backfilled marker rides the seed"
    );
    assert_eq!(rebuilt.boundary, Some(3), "index against the stored rows");
}

/// Precedent #156: a summary is a `Role::System` row, but it is real
/// conversation state — a rebuild that drops "the system message" (the
/// pre-#158 bug) loses all compacted history. The canonical rebuild keys on
/// the marker, not the role, so the summary survives the stored→seed cut.
#[test]
fn test_compact_contract_rebuild_156_summary_is_not_prompt() {
    let stored = vec![
        Message::system("stored prompt"),
        Message::system("[compacted: history]").with_compaction_summary(),
        Message::user("recent"),
    ];
    let rebuilt = rebuild_seed(&stored).expect("restorable");
    assert_eq!(rebuilt.seed.len(), 2);
    assert!(
        rebuilt.seed[0].is_compaction_summary,
        "#156: the compaction summary must survive the stored→seed cut"
    );
    assert_eq!(rebuilt.seed[0].content, "[compacted: history]");
    assert_eq!(rebuilt.seed[1].content, "recent");
}

/// Precedent #158: the boundary sits *after* the leading System prefix, and
/// rebuild must keep that order — prompt dropped (the runtime reassembles
/// it), summary as the seed head. A rebuild that keeps the prompt would
/// double it; one that reorders would break provider ordering.
#[test]
fn test_compact_contract_rebuild_158_boundary_after_prefix() {
    let stored = vec![
        Message::system("prompt-a"),
        Message::system("prompt-b"),
        Message::system("[compacted: old]").with_compaction_summary(),
        Message::user("u"),
        Message::assistant("a"),
    ];
    let rebuilt = rebuild_seed(&stored).expect("restorable");
    assert_eq!(
        rebuilt.boundary,
        Some(2),
        "marker index against stored rows"
    );
    let contents: Vec<&str> = rebuilt.seed.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(
        contents,
        vec!["[compacted: old]", "u", "a"],
        "#158: stored prompt dropped whole, summary leads, tail verbatim"
    );
}

/// Precedent #158, legacy-writer corollary: the AG-UI mirror persisted
/// `is_compaction_summary: false` (and emits no `compact_boundary` line)
/// before goal 413, so a compacted session it stored is
/// `[prompt, SUMMARY, tail…]` with both System rows unmarked. Dropping the
/// whole leading run would erase the folded history on resume; only the
/// first row is the stored prompt, and the recovered summary must be
/// marked so a later `apply` folds it rather than freezing it into the
/// prefix.
#[test]
fn test_compact_contract_rebuild_158_legacy_unmarked_summary_survives() {
    let stored = vec![
        Message::system("stored prompt"),
        Message::system("[compacted: history]"),
        Message::user("recent"),
        Message::assistant("reply"),
    ];
    let rebuilt = rebuild_seed(&stored).expect("restorable");
    assert_eq!(rebuilt.boundary, Some(1), "recovered summary index");
    let contents: Vec<&str> = rebuilt.seed.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(
        contents,
        vec!["[compacted: history]", "recent", "reply"],
        "legacy mirror summary must survive the stored→seed cut"
    );
    assert!(rebuilt.seed[0].is_compaction_summary, "re-labelled marker");
}

/// Precedent #160: the leading run of orphan `Role::Tool` results (their
/// issuing assistant was never persisted) must be dropped by the same
/// rebuild — a consumer that keeps them 400s on invariant #8; one with its
/// own bespoke slice arithmetic is the duplication #186 closes.
#[test]
fn test_compact_contract_rebuild_160_orphan_results_dropped_by_canonical_path() {
    let stored = vec![
        Message::system("stored prompt"),
        Message::tool_result("gone", "orphan"),
        Message::tool_result("gone2", "orphan2"),
        Message::user("turn"),
    ];
    let rebuilt = rebuild_seed(&stored).expect("restorable");
    let contents: Vec<&str> = rebuilt.seed.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(
        contents,
        vec!["turn"],
        "#160: orphan tool results before the first answerable row are dropped"
    );

    // Negative control: the pre-goal-413 hand-rolled framing (drop exactly
    // one system row, then drain leading Tools) matches for THIS shape —
    // the contract forbids the duplication, not this one transcript. What
    // the single path buys is that the exact rule (whole prefix run, marker
    // located anywhere, empty-assistant filter) lives in one place.
    let mut hand_rolled = stored.clone();
    hand_rolled.remove(0);
    let keep = hand_rolled
        .iter()
        .position(|m| m.role != Role::Tool)
        .unwrap_or(hand_rolled.len());
    hand_rolled.drain(..keep);
    assert_eq!(hand_rolled, rebuilt.seed);
}

/// Precedent #161: the marker round-trips the persisted→runtime conversion.
/// `entry_to_message` used to hardcode `is_compaction_summary = false`,
/// erasing the boundary on load; the round-trip must preserve it, and an
/// old row without the field still defaults to false.
#[test]
fn test_compact_contract_rebuild_161_marker_survives_entry_to_message() {
    use recursive::session::TranscriptEntry;
    let mut entry = TranscriptEntry {
        uuid: "u".into(),
        parent_uuid: None,
        source_tool_assistant_uuid: None,
        id: "msg_001".into(),
        parent_id: None,
        role: "system".into(),
        content: "[compacted: old]".into(),
        tool_calls: vec![],
        tool_call_id: None,
        reasoning_content: None,
        usage: None,
        timestamp: "2026-01-01T00:00:00Z".into(),
        audit: None,
        step: None,
        is_compaction_summary: true,
    };
    assert!(
        recursive::session::entry_to_message(entry.clone()).is_compaction_summary,
        "#161: the persisted boundary must survive entry_to_message"
    );
    entry.is_compaction_summary = false;
    assert!(
        !recursive::session::entry_to_message(entry).is_compaction_summary,
        "plain rows stay unmarked"
    );
}
