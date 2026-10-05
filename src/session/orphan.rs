//! Orphan tool-call detection types (Goal 153).
//!
//! Describes a tool call that was dispatched but never completed
//! (no matching `tool` result message in the transcript).
//!
//! Split from `session.rs` during the Goal 221 module refactor.

/// Result content that stands in for an orphan tool call the caller chose
/// *not* to re-execute. It answers the call so the resumed seed stays a
/// valid tool-call/tool-result chain (invariant #8) instead of hitting the
/// provider's "tool_use without tool_result" HTTP 400.
pub const ORPHAN_SKIPPED_RESULT: &str = "[interrupted: no result recorded]";

/// Prefix on the result content of an orphan tool call whose re-execution
/// failed. The error text is appended after it so the model can react
/// instead of the resume dying on the spot.
pub const ORPHAN_REDO_FAILED_PREFIX: &str = "[interrupted: redo failed] ";

/// Goal-153: describes a tool call that was dispatched but never completed
/// (no matching `tool` result message in the transcript).
#[derive(Debug, Clone)]
pub struct OrphanToolCall {
    /// `id` field of the assistant `TranscriptEntry` that issued the call.
    pub assistant_msg_id: String,
    /// The `id` of the tool call itself (matches `tool_call_id` on the
    /// expected — but missing — tool result message).
    pub tool_call_id: String,
    /// The name of the tool that was called.
    pub tool_name: String,
    /// The original call, verbatim from the transcript. Carries the
    /// arguments needed to re-execute the call under `--orphans=redo`.
    pub call: crate::llm::ToolCall,
    /// BLAKE3 of canonical JSON of the call arguments (for drift detection).
    pub args_hash: String,
    /// Side-effect class, determined from the current registry. Falls back
    /// to `External` when the tool is no longer registered (registry
    /// drifted since the session was saved) so `redo` always shows the
    /// conservative warning.
    pub side_effect_at_call: crate::tools::ToolSideEffect,
}

/// The `Vec<Message>` counterpart of
/// [`SessionReader::scan_orphan_tool_calls`](crate::session::SessionReader::scan_orphan_tool_calls).
///
/// `replay --resume-from N` seeds a run from an in-memory slice of a
/// transcript file rather than a session directory, so it cannot use the
/// on-disk scanner. The detection rules are identical: only the *last*
/// assistant message carrying `tool_calls` is examined (a crash leaves the
/// tail broken) and a call is an orphan when no later `tool` message
/// answers its id. Tools missing from `registry` fall back to `External`.
pub fn scan_orphan_tool_calls_in_messages(
    messages: &[crate::message::Message],
    registry: &crate::tools::ToolRegistry,
) -> Vec<OrphanToolCall> {
    use crate::message::Role;

    let Some((asst_idx, asst)) = messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, m)| m.role == Role::Assistant && !m.tool_calls.is_empty())
    else {
        return Vec::new();
    };

    // The result window is the tail from the issuing assistant onward; the
    // anchor itself is not a `tool` message, so it drops out below.
    let answered: std::collections::HashSet<&str> = messages[asst_idx..]
        .iter()
        .filter(|m| m.role == Role::Tool)
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();

    let mut orphans = Vec::new();
    for tc in &asst.tool_calls {
        if answered.contains(tc.id.as_str()) {
            continue;
        }
        let side_effect_at_call = registry
            .get(&tc.name)
            .map(|t| t.side_effect_class())
            .unwrap_or(crate::tools::ToolSideEffect::External);
        let args_hash = blake3::hash(tc.arguments.to_string().as_bytes())
            .to_hex()
            .to_string();
        orphans.push(OrphanToolCall {
            // In-memory messages carry no persistence id; no caller reads
            // it (the tool-call id is the addressing key).
            assistant_msg_id: String::new(),
            tool_call_id: tc.id.clone(),
            tool_name: tc.name.clone(),
            call: tc.clone(),
            args_hash,
            side_effect_at_call,
        });
    }
    orphans
}

/// The `Vec<Message>` counterpart of
/// [`SessionReader::load_messages_with_orphan_results`](crate::session::SessionReader::load_messages_with_orphan_results):
/// splice `answers` in as `tool` results so the seeded transcript stays a
/// paired tool-call/tool-result chain (invariant #8).
///
/// Answers land after the last `tool` message that follows the issuing
/// assistant message (or straight after that assistant message when the
/// crash left no result at all); with no assistant `tool_calls` anywhere
/// they trail the list. An empty `answers` returns the list unchanged.
pub fn splice_orphan_results(
    mut messages: Vec<crate::message::Message>,
    answers: &[(String, String)],
) -> Vec<crate::message::Message> {
    if answers.is_empty() {
        return messages;
    }
    let insert_at = orphan_result_insert_at_messages(&messages);
    let synthetic = answers
        .iter()
        .map(|(id, content)| crate::message::Message::tool_result(id.clone(), content.clone()));
    messages.splice(insert_at..insert_at, synthetic);
    messages
}

/// Index the orphan answers are inserted at, mirroring
/// `reader::orphan_result_insert_at` for the in-memory representation.
fn orphan_result_insert_at_messages(messages: &[crate::message::Message]) -> usize {
    use crate::message::Role;
    let Some(asst_idx) = messages
        .iter()
        .rposition(|m| m.role == Role::Assistant && !m.tool_calls.is_empty())
    else {
        return messages.len();
    };
    // Same shape as `reader::orphan_result_insert_at`: after the anchor's
    // own message (`skip(1)`), the last recorded result wins.
    match messages[asst_idx..]
        .iter()
        .skip(1)
        .rposition(|m| m.role == Role::Tool)
    {
        Some(offset) => asst_idx + 2 + offset,
        None => asst_idx + 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolCall;
    use crate::message::{Message, Role};

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: serde_json::json!({"path": "a.txt"}),
        }
    }

    fn assistant_with(calls: Vec<ToolCall>) -> Message {
        Message::assistant_with_tool_calls("calling", calls)
    }

    #[test]
    fn scan_in_messages_detects_unanswered_call() {
        let reg = crate::tools::ToolRegistry::local();
        let msgs = vec![
            Message::user("go"),
            assistant_with(vec![call("tc-1", "Read")]),
        ];
        let orphans = scan_orphan_tool_calls_in_messages(&msgs, &reg);
        assert_eq!(orphans.len(), 1, "one unanswered call is an orphan");
        assert_eq!(orphans[0].tool_call_id, "tc-1");
        assert_eq!(orphans[0].tool_name, "Read");
        assert!(!orphans[0].args_hash.is_empty(), "args must be hashed");
        // Unregistered tool (empty registry) → conservative External class.
        assert_eq!(
            orphans[0].side_effect_at_call,
            crate::tools::ToolSideEffect::External
        );
    }

    #[test]
    fn scan_in_messages_returns_empty_when_all_answered() {
        let reg = crate::tools::ToolRegistry::local();
        let msgs = vec![
            Message::user("go"),
            assistant_with(vec![call("tc-1", "Read")]),
            Message::tool_result("tc-1", "contents"),
        ];
        assert!(scan_orphan_tool_calls_in_messages(&msgs, &reg).is_empty());
    }

    #[test]
    fn scan_in_messages_reports_only_the_unpaired_call_of_a_batch() {
        let reg = crate::tools::ToolRegistry::local();
        let msgs = vec![
            Message::user("go"),
            assistant_with(vec![call("tc-1", "Read"), call("tc-2", "Bash")]),
            Message::tool_result("tc-1", "contents"),
        ];
        let orphans = scan_orphan_tool_calls_in_messages(&msgs, &reg);
        assert_eq!(orphans.len(), 1, "only tc-2 lacks a result");
        assert_eq!(orphans[0].tool_call_id, "tc-2");
    }

    #[test]
    fn scan_in_messages_returns_empty_without_assistant_calls() {
        let reg = crate::tools::ToolRegistry::local();
        let msgs = vec![
            Message::user("go"),
            Message::assistant("plain reply"),
            Message::tool_result("tc-1", "stray"),
        ];
        assert!(
            scan_orphan_tool_calls_in_messages(&msgs, &reg).is_empty(),
            "no assistant tool_calls → nothing to pair against"
        );
    }

    #[test]
    fn scan_in_messages_anchors_on_the_last_assistant_with_calls() {
        let reg = crate::tools::ToolRegistry::local();
        // A final assistant message without tool_calls has nothing to pair,
        // so the unanswered call on the earlier assistant is still the orphan.
        let msgs = vec![
            Message::user("go"),
            assistant_with(vec![call("tc-1", "Read")]),
            Message::assistant("thinking"),
        ];
        let orphans = scan_orphan_tool_calls_in_messages(&msgs, &reg);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].tool_call_id, "tc-1");
    }

    #[test]
    fn scan_in_messages_handles_a_seed_that_starts_on_the_assistant() {
        // `replay --resume-from N` can slice the transcript so the assistant
        // call is the first message: the answered-lookup must then be empty,
        // not skip a message (the `asst_idx + 1` slice start must hold at 0).
        let reg = crate::tools::ToolRegistry::local();
        let msgs = vec![assistant_with(vec![call("tc-1", "Read")])];
        let orphans = scan_orphan_tool_calls_in_messages(&msgs, &reg);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].tool_call_id, "tc-1");
    }

    #[test]
    fn splice_answers_every_unpaired_call() {
        let msgs = vec![
            Message::user("go"),
            assistant_with(vec![call("tc-1", "Read"), call("tc-2", "Bash")]),
        ];
        let spliced = splice_orphan_results(
            msgs,
            &[
                ("tc-1".to_string(), ORPHAN_SKIPPED_RESULT.to_string()),
                ("tc-2".to_string(), ORPHAN_SKIPPED_RESULT.to_string()),
            ],
        );
        assert_eq!(spliced.len(), 4, "user + assistant + 2 synthetic results");
        assert_eq!(spliced[2].role, Role::Tool);
        assert_eq!(spliced[2].tool_call_id.as_deref(), Some("tc-1"));
        assert_eq!(spliced[2].content, ORPHAN_SKIPPED_RESULT);
        assert_eq!(spliced[3].tool_call_id.as_deref(), Some("tc-2"));
    }

    #[test]
    fn splice_follows_a_partial_result_batch() {
        // tc-1's result survived the crash; the answer for tc-2 must land
        // *after* it, or the chain becomes `assistant, tool(2), tool(1)`.
        let msgs = vec![
            Message::user("go"),
            assistant_with(vec![call("tc-1", "Read"), call("tc-2", "Bash")]),
            Message::tool_result("tc-1", "recorded"),
        ];
        let spliced = splice_orphan_results(
            msgs,
            &[("tc-2".to_string(), ORPHAN_SKIPPED_RESULT.to_string())],
        );
        assert_eq!(spliced.len(), 4);
        assert_eq!(spliced[2].tool_call_id.as_deref(), Some("tc-1"));
        assert_eq!(spliced[2].content, "recorded");
        assert_eq!(spliced[3].tool_call_id.as_deref(), Some("tc-2"));
    }

    #[test]
    fn splice_with_no_answers_is_identity() {
        let msgs = vec![
            Message::user("go"),
            assistant_with(vec![call("tc-1", "Read")]),
        ];
        assert_eq!(splice_orphan_results(msgs.clone(), &[]), msgs);
    }

    #[test]
    fn splice_trails_the_list_when_there_is_no_assistant_call() {
        let msgs = vec![Message::user("hi"), Message::assistant("hello")];
        let spliced = splice_orphan_results(msgs, &[("tc-1".to_string(), "late".to_string())]);
        assert_eq!(spliced.len(), 3);
        assert_eq!(spliced[2].tool_call_id.as_deref(), Some("tc-1"));
    }

    #[test]
    fn orphan_insert_index_anchors_on_the_last_assistant_with_calls() {
        // Pins the index arithmetic: no result → right after the assistant,
        // one result → after it, a partial batch → after its *last* entry, and
        // a trailing assistant without tool_calls is never the anchor.
        use super::orphan_result_insert_at_messages as insert_at;

        let no_result = vec![Message::user("u"), assistant_with(vec![call("tc", "Read")])];
        assert_eq!(insert_at(&no_result), 2);

        let one_result = vec![
            Message::user("u"),
            assistant_with(vec![call("tc", "Read")]),
            Message::tool_result("tc", "out"),
        ];
        assert_eq!(insert_at(&one_result), 3);

        let trailing_plain_assistant = vec![
            Message::user("u"),
            assistant_with(vec![call("tc", "Read")]),
            Message::tool_result("tc", "out"),
            Message::assistant("done"),
        ];
        assert_eq!(
            insert_at(&trailing_plain_assistant),
            3,
            "the trailing assistant issued no calls, so it is not the anchor"
        );

        let partial_batch = vec![
            Message::user("u"),
            assistant_with(vec![call("a", "Read"), call("b", "Read")]),
            Message::tool_result("a", "out"),
            Message::tool_result("b", "out"),
        ];
        assert_eq!(
            insert_at(&partial_batch),
            4,
            "answers go after the whole partial result batch"
        );

        let none = vec![Message::user("u")];
        assert_eq!(insert_at(&none), 1);
    }
}
