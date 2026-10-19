//! Canonical rebuild from the persisted compaction boundary (goal 413).
//!
//! Goal 411 made [`Compactor::apply`](super::Compactor::apply) the single
//! *write* path; this module is its consumer-side twin: the single *rebuild*
//! path. The boundary `apply` splices into the transcript is a
//! [`Role::System`] row with `is_compaction_summary` set, right after the
//! leading System prefix ([`super::Compactor::leading_system_prefix_len`]) —
//! and that same row *is* the persisted marker: every writer that stores
//! `Message`s (`SessionWriter`, the AG-UI mirror, the legacy-transcript
//! rewrite) round-trips the bit through
//! [`crate::session::TranscriptEntry`]'s `is_compaction_summary` field, so no
//! second checkpoint format is needed and old transcripts (field absent)
//! stay loadable. The g157 `compact_boundary` line is the same boundary in
//! session files; readers backfill the bit from it, so transcripts written
//! before the field existed keep their summary too. Files that carry
//! *neither* (the AG-UI mirror before this goal) are recovered by the
//! second-row rule in [`rebuild_seed`].
//!
//! Every consumer that turns stored rows back into a model seed — CLI resume
//! ([`crate::session::SessionReader::load_messages*`]), AG-UI thread reload
//! (`crate::http::agui::load_thread_seed`), HTTP cold load
//! (`crate::http::cold_load::normalize_stored_transcript`) — must rebuild
//! through [`rebuild_seed`]. The only cutting a consumer may do is honouring
//! the persisted boundary declaration (see below); everything else is framing
//! and belongs here.
//!
//! The framing rules that used to live at each call site are expressed here
//! once (issue #186; precedents #156 / #158 / #160 / #161):
//!
//! 1. locate the boundary: the first `is_compaction_summary` row — the same
//!    rule the runtime's post-compaction paths use to find the summary in
//!    memory (issue #158: a summary is a `Role::System` row but is real
//!    conversation state, never part of the prompt run);
//! 2. drop the stored rebuilt prompt: the leading non-summary `Role::System`
//!    run — the runtime reassembles its own prompt, and a second one on the
//!    seed would double it. The run stops at the summary, so the summary
//!    stays as the seed head. One legacy shape needs a finer cut: the AG-UI
//!    mirror wrote `is_compaction_summary: false` (and no
//!    `compact_boundary` line) before this goal, so such a file stores
//!    `[prompt, SUMMARY, tail…]` with *both* System rows unmarked. Only the
//!    first row of the run is the stored prompt — the runtime prepends
//!    exactly one (`RuntimeBuilder::build`), and `apply` splices the summary
//!    immediately after it — so a second unmarked row can only be that
//!    legacy summary and is kept (re-labelled, observably) instead of being
//!    dropped with the prompt;
//! 3. drop the leading run of orphan `Role::Tool` results — their issuing
//!    assistant was never persisted, so no later row can answer to them
//!    (invariant #8; providers reject the seed with HTTP 400);
//! 4. drop empty-assistant rows via [`crate::message::is_empty_assistant`]
//!    (#168) — a poisoned row would 400 every later request.
//!
//! Every row from the summary on — the summary itself, re-injected
//! attachments, the preserved tail — is kept verbatim, so for an input that
//! starts at the boundary the seed equals the in-memory transcript a session
//! had right after its compaction: the contract the goal-413 tests pin as
//! 「重建结果 ≡ 压缩后内存态」.
//!
//! ## Why the cut itself is not here
//!
//! A marker row on its own does *not* prove that the rows before it are
//! superseded, so this function must not slice at one:
//!
//! * the g157 `compact_boundary` line is the persisted *declaration* of what
//!   was folded, and honouring it — cut at the last declaration, then mark
//!   the row right after it when a pre-goal-413 writer left the bit off — is
//!   [`rebuild_declared_seed`], the one place that rule lives. Every reader
//!   of an append-only file (`SessionReader::load_messages*`, AG-UI thread
//!   reload) calls it; none slices on its own, so none can drop a legacy
//!   summary together with the stored prompt (#158);
//! * intra-turn compaction is applied to the kernel's copy-on-write clone, so
//!   the wrapper can only *append* the summary
//!   (`AgentRuntime::mark_transcript_rewritten` documents this): a persisted
//!   snapshot of such a turn is `[…history…, SUMMARY, …turn tail…]`, where
//!   the rows before the summary are the *preserved* tail, not folded
//!   history. Slicing at the marker there would silently delete the most
//!   recent conversation on the next resume.
//!
//! So the canonical rebuild starts after the declaration and frames what is
//! left; consumers that read an append-only file must honour the declaration
//! before calling it.

use crate::message::{Message, Role};

/// The post-compaction boundary state of a stored transcript.
///
/// `boundary` is the position of the summary row in the stored transcript —
/// audit/telemetry data (e.g. for resume pickers). It is not a cut this
/// module performs: a persisted declaration has to be honoured by the caller
/// first (see [`rebuild_declared_seed`]), and it is why this field is
/// reported against the *whole* stored vector there (`Some(0)`/`None` when
/// the caller already cut at the declaration).
#[derive(Debug, Clone, PartialEq)]
pub struct RebuiltSeed {
    /// The rebuilt model seed: conversation state (summary first when the
    /// session ever compacted), with the stored prompt and orphan tool
    /// results removed.
    pub seed: Vec<Message>,
    /// Index of the compaction-summary row in the stored transcript, when
    /// one is present or recoverable (a legacy mirror row is recovered by
    /// the second-row rule). Audit/telemetry data, e.g. for resume pickers.
    pub boundary: Option<usize>,
}

/// Rebuild a model seed from stored transcript rows through the canonical
/// compaction boundary.
///
/// The steps, in order — the order matters, see [`RebuiltSeed`]:
///
/// 1. locate the boundary: the first `is_compaction_summary` row;
/// 2. drop the stored rebuilt prompt: the leading non-summary `Role::System`
///    run (the run stops at the summary, which stays). When no row is marked
///    and the run is longer than one row, the second row is a legacy summary
///    — it is kept and re-labelled instead of being dropped with the prompt;
/// 3. drop empty-assistant rows (#168), preserving tool-call pairing;
/// 4. drop the leading orphan `Role::Tool` run (invariant #8).
///
/// `stored` must already be cut at the persisted boundary when one was
/// declared (the g157 `compact_boundary` line): a reader of an append-only
/// file calls [`rebuild_declared_seed`] instead of this function directly,
/// because the declaration — not the marker row — says which rows were
/// folded. See the module docs for why slicing at the marker here would
/// delete un-summarised history.
///
/// `None` when nothing restorable remains (empty input, prompt-only,
/// orphan-results-only): the caller keeps its "no session to restore"
/// semantics instead of materialising a ghost session.
pub fn rebuild_seed(stored: &[Message]) -> Option<RebuiltSeed> {
    let marked = stored.iter().position(|m| m.is_compaction_summary);
    // The same rule `apply` uses in memory to keep the prompt prefix verbatim:
    // rows before the split that are not prompt (orphan tool results, stray
    // rows) are handled by their own drop rules below, so a transcript that
    // never compacted loses only what it must.
    let prefix_len = super::Compactor::leading_system_prefix_len(stored);
    // A legacy mirror file carries neither the marker bit nor a boundary
    // line, so a compacted transcript reaches us as `[prompt, SUMMARY, tail…]`
    // with both System rows unmarked. The runtime prepends exactly one prompt
    // and `apply` splices the summary right after it, so the second row can
    // only be that summary; dropping the run whole (#158's failure class)
    // would erase the folded history on resume.
    let legacy_summary = marked.is_none() && prefix_len >= 2;
    let skip = if legacy_summary { 1 } else { prefix_len };
    let mut msgs: Vec<Message> = stored
        .iter()
        .skip(skip)
        .filter(|m| !crate::message::is_empty_assistant(m))
        .cloned()
        .collect();
    if legacy_summary {
        if let Some(summary) = msgs.first_mut() {
            if !summary.is_compaction_summary {
                // Owner ruling: a synthesised marker is a degradation, so it
                // must be observable — a genuine second prompt run would be
                // silently reinterpreted as a summary otherwise.
                tracing::warn!(
                    "legacy transcript has a second unmarked system row under the stored \
                     prompt; treating it as the compaction summary so the folded history \
                     survives the rebuild"
                );
                summary.is_compaction_summary = true;
            }
        }
    }
    drop_leading_orphan_tool_results(&mut msgs);
    if msgs.is_empty() {
        None
    } else {
        Some(RebuiltSeed {
            seed: msgs,
            boundary: marked.or(legacy_summary.then_some(1)),
        })
    }
}

/// Drop the leading run of `Role::Tool` results (in place).
///
/// An unsendable head: the assistant that issued these calls is not in the
/// seed, so no later row can answer to it (invariant #8).
fn drop_leading_orphan_tool_results(msgs: &mut Vec<Message>) {
    let keep = msgs
        .iter()
        .position(|m| m.role != Role::Tool)
        .unwrap_or(msgs.len());
    msgs.drain(..keep);
}

/// Rebuild a seed from a whole append-only persisted transcript, honouring
/// its last g157 `compact_boundary` declaration.
///
/// `stored` is every row in file order; `after_boundary` is the index of the
/// first row after the last declaration, or `None` when the file carries
/// none. This is the **one** place the declaration rule lives — resume
/// ([`crate::session::SessionReader::load_messages`] and its orphan-splice
/// variant) and AG-UI thread reload both call it, so neither can re-introduce
/// #158 by dropping the legacy summary along with the stored prompt.
///
/// The row right after a declaration is the summary
/// [`super::Compactor::apply`] spliced in. Writers before goal 413 persisted
/// it without the marker bit, so an unmarked `Role::System` head is marked
/// here — observably, per the owner ruling that every degradation must
/// leave a WARNING-or-louder trace.
///
/// `boundary` in the result is reported against the full `stored` vector
/// (the declaration index plus the relative marker position).
pub fn rebuild_declared_seed(
    stored: &[Message],
    after_boundary: Option<usize>,
) -> Option<RebuiltSeed> {
    let Some(at) = after_boundary else {
        return rebuild_seed(stored);
    };
    let mut rows = stored.get(at..)?.to_vec();
    let head = rows.first_mut()?;
    // A `Role::System` head is the summary; anything else is a crash between
    // the declaration and the summary row, which must not be relabelled.
    if head.role == Role::System && !head.is_compaction_summary {
        tracing::warn!(
            summary_chars = head.content.chars().count(),
            "compaction boundary declaration has no persisted summary marker; \
             marking the first post-boundary system row as the summary"
        );
        head.is_compaction_summary = true;
    }
    let mut rebuilt = rebuild_seed(&rows)?;
    if let Some(relative) = rebuilt.boundary {
        rebuilt.boundary = Some(at + relative);
    }
    Some(rebuilt)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(text: &str) -> Message {
        Message::system(format!("[compacted: {text}]")).with_compaction_summary()
    }

    #[test]
    fn rebuild_keeps_summary_first_and_tail_verbatim() {
        let stored = vec![
            Message::system("stored prompt"),
            summary("older history"),
            Message::user("recent"),
            Message::assistant("reply"),
        ];
        let out = rebuild_seed(&stored).expect("restorable");
        assert_eq!(out.boundary, Some(1));
        let contents: Vec<&str> = out.seed.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(
            contents,
            vec!["[compacted: older history]", "recent", "reply"]
        );
        assert!(out.seed[0].is_compaction_summary);
    }

    #[test]
    fn rebuild_recovers_a_legacy_unmarked_summary_under_the_prompt() {
        // Before goal 413 the AG-UI mirror wrote `is_compaction_summary:
        // false` and no `compact_boundary` line, so a compacted session it
        // stored is `[prompt, SUMMARY, tail…]` with both System rows
        // unmarked. Dropping the whole leading run would erase the folded
        // history; only the first row is the stored prompt.
        let stored = vec![
            Message::system("stored prompt"),
            Message::system("[compacted: older history]"),
            Message::user("recent"),
            Message::assistant("reply"),
        ];
        let out = rebuild_seed(&stored).expect("restorable");
        assert_eq!(
            out.boundary,
            Some(1),
            "the recovered summary is the boundary"
        );
        let contents: Vec<&str> = out.seed.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(
            contents,
            vec!["[compacted: older history]", "recent", "reply"]
        );
        assert!(
            out.seed[0].is_compaction_summary,
            "re-labelled so a later `apply` folds it instead of freezing it \
             into the prompt prefix"
        );
    }

    #[test]
    fn rebuild_legacy_recovery_keeps_reinjected_system_rows_after_the_summary() {
        // `apply` splices re-injected attachments right after the summary, so
        // the legacy run is `[prompt, SUMMARY, attachment, …]`; the recovery
        // skips only the prompt and keeps the rest of the run verbatim.
        let stored = vec![
            Message::system("stored prompt"),
            Message::system("[compacted: older history]"),
            Message::system("<!-- skill:pdf injected -->"),
            Message::user("recent"),
        ];
        let out = rebuild_seed(&stored).expect("restorable");
        let contents: Vec<&str> = out.seed.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(
            contents,
            vec![
                "[compacted: older history]",
                "<!-- skill:pdf injected -->",
                "recent",
            ]
        );
        assert!(out.seed[0].is_compaction_summary);
        assert!(!out.seed[1].is_compaction_summary);
    }

    #[test]
    fn rebuild_with_the_marker_present_drops_the_whole_prompt_run() {
        // Negative control for the legacy rule: a marked summary says the run
        // before it is all prompt, so every unmarked System row there goes.
        let stored = vec![
            Message::system("prompt-a"),
            Message::system("prompt-b"),
            summary("older"),
            Message::user("recent"),
        ];
        let out = rebuild_seed(&stored).expect("restorable");
        assert_eq!(out.boundary, Some(2));
        let contents: Vec<&str> = out.seed.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(contents, vec!["[compacted: older]", "recent"]);
    }

    #[test]
    fn rebuild_without_a_boundary_drops_only_prompt_and_orphans() {
        // A legacy transcript that never compacted: the stored prompt and a
        // leading orphan tool result go, every real row stays in order.
        let stored = vec![
            Message::system("stored prompt"),
            Message::tool_result("gone", "orphan"),
            Message::user("turn"),
            Message::assistant("reply"),
        ];
        let out = rebuild_seed(&stored).expect("restorable");
        assert_eq!(out.boundary, None);
        let contents: Vec<&str> = out.seed.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(contents, vec!["turn", "reply"]);
    }

    #[test]
    fn rebuild_empty_assistant_rows_are_filtered() {
        let stored = vec![
            Message::system("prompt"),
            Message::user("u"),
            Message::assistant(""),
            Message::assistant("real"),
        ];
        let out = rebuild_seed(&stored).expect("restorable");
        let contents: Vec<&str> = out.seed.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(contents, vec!["u", "real"]);
    }

    #[test]
    fn rebuild_returns_none_when_nothing_restorable_remains() {
        assert!(rebuild_seed(&[]).is_none());
        assert!(rebuild_seed(&[Message::system("prompt only")]).is_none());
        assert!(rebuild_seed(&[Message::tool_result("gone", "orphan")]).is_none());
        assert!(
            rebuild_seed(&[Message::system("p"), Message::tool_result("g", "o")]).is_none(),
            "prompt + orphan results is not a restorable conversation"
        );
    }

    #[test]
    fn rebuild_does_not_slice_at_a_marker_that_trails_the_history() {
        // Intra-turn compaction is applied to the kernel's copy-on-write
        // clone, so the wrapper only *appends* the summary: a persisted
        // snapshot of such a turn is `[…history…, SUMMARY, tail]`. The rows
        // before the marker are the preserved tail, not folded history —
        // slicing at them (the bug this test pins) would delete the most
        // recent conversation on the next resume.
        let stored = vec![
            Message::system("stored prompt"),
            Message::user("turn 1"),
            Message::assistant("answer 1"),
            Message::user("turn 2"),
            Message::assistant("answer 2"),
            summary("turn 1-2 prefix"),
            Message::assistant("answer 3"),
        ];
        let out = rebuild_seed(&stored).expect("restorable");
        assert_eq!(out.boundary, Some(5));
        let contents: Vec<&str> = out.seed.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(
            contents,
            vec![
                "turn 1",
                "answer 1",
                "turn 2",
                "answer 2",
                "[compacted: turn 1-2 prefix]",
                "answer 3",
            ],
            "a trailing marker declares nothing about the rows before it"
        );
        assert!(out.seed[4].is_compaction_summary);
    }

    #[test]
    fn rebuild_positions_against_the_stored_vector_are_audit_data() {
        // A stored transcript may carry the boundary mid-file while orphan
        // tool results precede it; the boundary index is audit data against
        // the *stored* vector and must not shift.
        let stored = vec![
            Message::system("stored prompt"),
            Message::tool_result("gone", "orphan"),
            summary("older"),
            Message::user("recent"),
        ];
        let out = rebuild_seed(&stored).expect("restorable");
        assert_eq!(out.boundary, Some(2));
        assert_eq!(out.seed.len(), 2);
        assert!(out.seed[0].is_compaction_summary);
        assert_eq!(out.seed[1].content, "recent");
    }

    #[test]
    fn rebuild_declared_seed_marks_a_legacy_summary_after_the_boundary() {
        // Review BLOCKER probe: an append-only thread file is
        // `[prompt, u1, a1, <declaration>, SUMMARY(unmarked), u2, a2]`. The
        // declaration's first row is the summary a pre-goal-413 writer left
        // unmarked; dropping it with the prompt would erase every folded
        // turn (the #158 failure class, third consumer).
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
        assert_eq!(contents, vec!["[compacted: u1 a1]", "u2", "a2"]);
        assert!(
            rebuilt.seed[0].is_compaction_summary,
            "the synthesised marker must be observable in the seed"
        );
        assert_eq!(rebuilt.boundary, Some(3), "boundary is against `stored`");
    }

    #[test]
    fn rebuild_declared_seed_without_a_declaration_is_the_plain_rebuild() {
        let stored = vec![
            Message::system("stored prompt"),
            Message::system("[compacted: old]").with_compaction_summary(),
            Message::user("u"),
        ];
        assert_eq!(
            rebuild_declared_seed(&stored, None),
            rebuild_seed(&stored),
            "no declaration means no cut and no backfill"
        );
    }

    #[test]
    fn rebuild_declared_seed_at_eof_has_nothing_to_restore() {
        let stored = vec![Message::system("stored prompt")];
        assert!(rebuild_declared_seed(&stored, Some(1)).is_none());
        assert!(rebuild_declared_seed(&stored, Some(9)).is_none());
    }

    #[test]
    fn rebuild_declared_seed_without_a_summary_row_reports_no_boundary() {
        // A crash between the declaration and the summary row leaves ordinary
        // conversation rows after it; they are real history, and there is no
        // boundary row to report.
        let stored = vec![Message::user("u"), Message::assistant("a")];
        let rebuilt = rebuild_declared_seed(&stored, Some(0)).expect("restorable");
        assert_eq!(rebuilt.boundary, None);
        assert_eq!(rebuilt.seed.len(), 2);
    }
}
