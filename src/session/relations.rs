//! Relation tracing over a session's history (issue #131, borrowed from DSH
//! `session-query`'s `tracing.ts`).
//!
//! "Why does the history read like this now?" is a question plain full-text
//! search cannot answer, because the answer is a relation between entries, not
//! an entry. Two relations are derivable from what recursive persists:
//!
//! - **replacement chain** — a cross-turn compaction folds the *oldest*
//!   messages of the model context into a summary written behind a
//!   `compact_boundary` marker in the JSONL. The folded messages are *replaced
//!   by* that summary: they are still on disk (the user can read them) but no
//!   longer part of the model-facing context. [`replacements`] rebuilds the
//!   chain from the markers, and [`trace_event`] answers the question in both
//!   directions for one entry.
//! - **origin chain** — recursive persists no explicit fork lineage, so
//!   [`SessionTrace::same_origin`] reports the derived signal instead: sessions
//!   whose first message uuid matches (a verbatim fork/copy of the same log).
//!
//! Both are derived from the logs on every call; nothing here is stored.

use crate::error::Result;
use crate::session::index::SessionIndex;
use crate::session::serialize::LoadedEntry;

/// One compaction: the `replaced` oldest messages the model context still
/// held were folded into the summary message at `summary_index`.
///
/// The folded block is the *oldest* part of the log, not the messages adjacent
/// to the marker: the compaction drains a prefix of the context and appends the
/// summary behind the marker, so the marker lands at the end of the history it
/// summarises. The folded messages occupy `[first_replaced, first_replaced +
/// replaced)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Replacement {
    /// Message index of the summary that now stands in for the folded range.
    /// Message indices count messages only — `compact_boundary` markers are not
    /// entries and do not shift them.
    pub summary_index: usize,
    /// Oldest folded message index (inclusive).
    pub first_replaced: usize,
    /// How many older messages the summary replaced.
    pub replaced: usize,
    /// Turn in which the compaction fired, when the marker recorded one.
    pub turn: Option<u32>,
}

/// The full relation picture of one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTrace {
    pub session_id: String,
    pub goal: String,
    pub status: String,
    /// Messages in the log (compaction markers excluded).
    pub message_count: usize,
    /// Compaction replacements, oldest first.
    pub replacements: Vec<Replacement>,
    /// Sessions that start from the same origin transcript.
    pub same_origin: Vec<String>,
}

/// The relation picture of one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventTrace {
    pub session_id: String,
    pub index: usize,
    pub entry_id: String,
    pub role: String,
    /// Set when this entry was folded into a later summary.
    pub superseded_by: Option<Replacement>,
    /// Set when this entry is a summary that replaced earlier messages.
    pub supersedes: Option<Replacement>,
}

/// Rebuild the replacement chain of a history (messages + boundary markers).
///
/// A marker's `removed` is `Compactor::apply_to_transcript`'s split: the
/// producer drains `transcript[..split]` — the *oldest* messages of the model
/// context — summarises exactly that prefix, and writes the summary after the
/// marker. The folded block is therefore a prefix of the log that only ever
/// moves forward, so the window is rebuilt with a cursor: each marker folds the
/// `removed` oldest messages that no earlier marker already folded.
pub fn replacements(history: &[LoadedEntry]) -> Vec<Replacement> {
    let mut out = Vec::new();
    let mut message_index = 0usize;
    // One past the last message an earlier marker folded.
    let mut first_unfolded = 0usize;
    for entry in history {
        match entry {
            LoadedEntry::Message(_) => message_index += 1,
            LoadedEntry::CompactBoundary { turn, removed } => {
                // Nothing was folded (a zero-count marker) — not a relation.
                if *removed == 0 {
                    continue;
                }
                // A marker can claim more than the log holds: a resumed run
                // re-seeds a longer transcript than the log it appends to. What
                // is derivable is the messages actually present before it, and
                // clamping keeps a marker from claiming its own summary.
                let replaced = (*removed).min(message_index.saturating_sub(first_unfolded));
                if replaced == 0 {
                    continue;
                }
                out.push(Replacement {
                    summary_index: message_index,
                    first_replaced: first_unfolded,
                    replaced,
                    turn: *turn,
                });
                first_unfolded += replaced;
            }
        }
    }
    out
}

/// Trace a whole session: its header, every compaction replacement, and the
/// sessions that share its origin transcript.
pub fn trace_session(index: &mut SessionIndex, session_id: &str) -> Result<SessionTrace> {
    let hit = index
        .session(session_id)?
        .ok_or_else(|| crate::error::Error::NotFound(format!("session {session_id}")))?;
    let history = index.transcript(session_id)?;
    Ok(SessionTrace {
        session_id: hit.session_id,
        goal: hit.goal,
        status: hit.status,
        message_count: history
            .iter()
            .filter(|e| matches!(e, LoadedEntry::Message(_)))
            .count(),
        replacements: replacements(&history),
        same_origin: index.same_origin(session_id)?,
    })
}

/// Trace one entry: what replaced it, and what it replaced.
pub fn trace_event(index: &mut SessionIndex, session_id: &str, at: usize) -> Result<EventTrace> {
    let history = index.transcript(session_id)?;
    let messages: Vec<&crate::session::TranscriptEntry> = history
        .iter()
        .filter_map(|e| match e {
            LoadedEntry::Message(m) => Some(m.as_ref()),
            LoadedEntry::CompactBoundary { .. } => None,
        })
        .collect();
    let entry = messages.get(at).ok_or_else(|| {
        crate::error::Error::NotFound(format!("event {at} of session {session_id}"))
    })?;

    let chain = replacements(&history);
    let superseded_by = chain
        .iter()
        .find(|r| at >= r.first_replaced && at < r.first_replaced + r.replaced)
        .copied();
    let supersedes = chain.iter().find(|r| r.summary_index == at).copied();

    Ok(EventTrace {
        session_id: session_id.to_string(),
        index: at,
        entry_id: entry.id.clone(),
        role: entry.role.clone(),
        superseded_by,
        supersedes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;
    use crate::session::index::SessionIndex;
    use crate::session::{SessionStatus, SessionWriter};
    use crate::test_util::IsolatedWorkspace;

    /// Write a session in the layout the producer writes: the whole
    /// pre-compaction transcript first, then the `compact_boundary` marker
    /// whose compaction folded its oldest 3 messages, then the summary the
    /// marker introduces, then the messages appended after it.
    ///
    /// The marker therefore sits at the *end* of the history it folds — placing
    /// it in front of the messages it replaced would not match
    /// `Compactor::apply_to_transcript`, which drains `transcript[..split]`.
    fn write_compacted_session(ws: &std::path::Path) -> String {
        let mut writer = SessionWriter::create(ws, "compacted goal", "m", "p").unwrap();
        for msg in [
            Message::user("u0".to_string()),
            Message::assistant("a0".to_string()),
            Message::user("u1".to_string()),
            Message::assistant("a1".to_string()),
        ] {
            writer.append(&msg, None, None).unwrap();
        }
        writer
            .write_compact_boundary(2, 3, Some("summary-uuid"))
            .unwrap();
        writer
            .append(
                &Message::user("[summary of earlier turns]".to_string()),
                None,
                None,
            )
            .unwrap();
        writer
            .append(&Message::assistant("after".to_string()), None, None)
            .unwrap();
        writer.finish(SessionStatus::Completed).unwrap();
        writer.session_id().to_string()
    }

    #[test]
    fn replacements_count_messages_only() {
        let env = IsolatedWorkspace::new();
        let session_id = write_compacted_session(env.path());
        let mut index = SessionIndex::open(env.path()).unwrap();
        index.refresh().unwrap();

        let history = index.transcript(&session_id).unwrap();
        let chain = replacements(&history);
        assert_eq!(
            chain,
            vec![Replacement {
                // 4 messages preceded the marker; the fold took the oldest 3
                // (u0/a0/u1), leaving a1 verbatim and the summary at index 4.
                summary_index: 4,
                first_replaced: 0,
                replaced: 3,
                turn: Some(2),
            }]
        );
    }

    #[test]
    fn trace_session_reports_header_and_chain() {
        let env = IsolatedWorkspace::new();
        let session_id = write_compacted_session(env.path());
        let mut index = SessionIndex::open(env.path()).unwrap();
        index.refresh().unwrap();

        let trace = trace_session(&mut index, &session_id).unwrap();
        assert_eq!(trace.session_id, session_id);
        assert_eq!(trace.goal, "compacted goal");
        assert_eq!(trace.status, "completed");
        assert_eq!(trace.message_count, 6, "boundary markers are not messages");
        assert_eq!(trace.replacements.len(), 1);
        assert!(trace.same_origin.is_empty());
    }

    #[test]
    fn trace_event_reports_both_directions() {
        let env = IsolatedWorkspace::new();
        let session_id = write_compacted_session(env.path());
        let mut index = SessionIndex::open(env.path()).unwrap();
        index.refresh().unwrap();

        let folded_window = Some(Replacement {
            summary_index: 4,
            first_replaced: 0,
            replaced: 3,
            turn: Some(2),
        });

        // A folded message knows which summary replaced it. The marker folded
        // the oldest three messages (indices 0..3).
        let folded = trace_event(&mut index, &session_id, 2).unwrap();
        assert_eq!(folded.role, "user");
        assert_eq!(folded.superseded_by, folded_window);
        assert!(folded.supersedes.is_none());

        // Index 3 (a1) is the message the compaction kept verbatim: it is still
        // in the model's context, so nothing superseded it.
        let kept = trace_event(&mut index, &session_id, 3).unwrap();
        assert!(
            kept.superseded_by.is_none(),
            "the kept tail must not be reported as folded"
        );

        // The summary at index 4 reports the reverse relation.
        let summary = trace_event(&mut index, &session_id, 4).unwrap();
        assert_eq!(summary.supersedes, folded_window);
        assert!(summary.superseded_by.is_none());

        // Index 5 (after the compaction) is untouched by the chain.
        let after = trace_event(&mut index, &session_id, 5).unwrap();
        assert!(after.supersedes.is_none());
        assert!(after.superseded_by.is_none());
        assert_eq!(after.entry_id, "msg_006");
    }

    #[tokio::test]
    async fn a_real_compaction_folds_the_oldest_messages() {
        use crate::event::{CompositeSink, EventSink, NullSink};
        use crate::llm::{Completion, MockProvider};
        use crate::session::SessionPersistenceSink;
        use crate::{AgentRuntime, Compactor};
        use std::sync::{Arc, Mutex};

        fn reply(text: &str) -> Completion {
            Completion {
                content: text.to_string(),
                tool_calls: vec![],
                finish_reason: Some("stop".to_string()),
                usage: None,
                reasoning_content: None,
            }
        }

        let env = IsolatedWorkspace::new();
        let writer = Arc::new(Mutex::new(
            SessionWriter::create(env.path(), "real compaction", "m", "p").unwrap(),
        ));
        // Same fixture as `tests/compact_boundary.rs`: a three-turn run whose
        // cross-turn compaction fires more than once, so the log carries a
        // multi-marker chain (observed: folds at turn 1 and turn 2).
        let llm = Arc::new(MockProvider::new(vec![
            reply("reply1"),
            reply("reply2"),
            reply("compact summary"),
            reply("reply3"),
            reply("compact summary"),
            reply("compact summary"),
        ]));
        let sink = Arc::new(CompositeSink::new(vec![
            Box::new(NullSink) as Box<dyn EventSink>,
            Box::new(SessionPersistenceSink::new(writer.clone())) as Box<dyn EventSink>,
        ]));
        let mut runtime = AgentRuntime::builder()
            .llm(llm)
            .event_sink(sink)
            .compactor(Compactor::new(1).keep_recent_n(2))
            .build()
            .unwrap();
        runtime.run("turn1").await.unwrap();
        runtime.run("turn2").await.unwrap();
        runtime.run("turn3").await.unwrap();
        drop(runtime);
        let session_id = {
            let mut writer = writer.lock().unwrap();
            writer.finish(SessionStatus::Completed).unwrap();
            writer.session_id().to_string()
        };

        let mut index = SessionIndex::open(env.path()).unwrap();
        index.refresh().unwrap();
        let trace = trace_session(&mut index, &session_id).unwrap();
        assert!(
            !trace.replacements.is_empty(),
            "the fixture must have compacted: {trace:?}"
        );

        let history = index.transcript(&session_id).unwrap();
        let messages: Vec<&crate::session::TranscriptEntry> = history
            .iter()
            .filter_map(|e| match e {
                LoadedEntry::Message(m) => Some(m.as_ref()),
                LoadedEntry::CompactBoundary { .. } => None,
            })
            .collect();

        for replacement in &trace.replacements {
            // The summary behind the marker records how many messages the
            // producer folded into it: the trace has to agree with it.
            let header = format!("[compacted: {} messages", replacement.replaced);
            assert!(
                messages[replacement.summary_index]
                    .content
                    .starts_with(&header),
                "summary at {} must record `{header}`: {replacement:?}",
                replacement.summary_index
            );
            assert!(
                replacement.first_replaced + replacement.replaced <= replacement.summary_index,
                "a folded window stays behind its own summary: {replacement:?}"
            );
        }

        // Compaction drains the oldest part of the context, so the first fold
        // starts at the very first message of the log.
        let first = trace.replacements[0];
        assert_eq!(
            first.first_replaced, 0,
            "the fold starts at the oldest message: {trace:?}"
        );
        assert_eq!(
            trace_event(&mut index, &session_id, 0)
                .unwrap()
                .superseded_by,
            Some(first)
        );

        // The newest message before the marker was still in the model's
        // context after this compaction, so *this* replacement did not fold it.
        // The bug this pins derived the window as `summary_index - removed` and
        // reported exactly that message as folded.
        assert_ne!(
            trace_event(&mut index, &session_id, first.summary_index - 1)
                .unwrap()
                .superseded_by,
            Some(first)
        );
    }

    #[test]
    fn trace_session_without_compaction_has_no_relations() {
        let env = IsolatedWorkspace::new();
        let mut writer = SessionWriter::create(env.path(), "plain", "m", "p").unwrap();
        writer
            .append(&Message::user("hello".to_string()), None, None)
            .unwrap();
        writer.finish(SessionStatus::Completed).unwrap();
        let session_id = writer.session_id().to_string();

        let mut index = SessionIndex::open(env.path()).unwrap();
        index.refresh().unwrap();
        let trace = trace_session(&mut index, &session_id).unwrap();
        assert!(trace.replacements.is_empty());
        assert_eq!(trace.message_count, 1);
    }

    #[test]
    fn zero_count_boundary_is_not_a_replacement() {
        let env = IsolatedWorkspace::new();
        let mut writer = SessionWriter::create(env.path(), "zero", "m", "p").unwrap();
        writer
            .append(&Message::user("hello".to_string()), None, None)
            .unwrap();
        writer.write_compact_boundary(1, 0, None).unwrap();
        writer
            .append(&Message::assistant("world".to_string()), None, None)
            .unwrap();
        writer.finish(SessionStatus::Completed).unwrap();
        let session_id = writer.session_id().to_string();

        let mut index = SessionIndex::open(env.path()).unwrap();
        index.refresh().unwrap();
        let trace = trace_session(&mut index, &session_id).unwrap();
        assert!(trace.replacements.is_empty());
        assert_eq!(trace.message_count, 2);
    }

    #[test]
    fn unknown_session_and_event_are_not_found() {
        let env = IsolatedWorkspace::new();
        write_compacted_session(env.path());
        let mut index = SessionIndex::open(env.path()).unwrap();
        index.refresh().unwrap();

        assert!(trace_session(&mut index, "missing").is_err());
        let session_id = index.sessions(10).unwrap()[0].session_id.clone();
        assert!(trace_event(&mut index, &session_id, 99).is_err());
    }
}
