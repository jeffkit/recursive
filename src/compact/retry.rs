//! PTL (prompt-too-long) retry helpers for compaction summarisation.
//!
//! When the compactor asks the provider to summarise an older transcript
//! slice, that very request may itself exceed the provider's context window.
//! This module provides best-effort escape hatches: drop the oldest message
//! groups (bounded by User/System boundaries) and retry up to
//! [`MAX_PTL_RETRIES`] times before giving up.

use crate::message::{Message, Role};

/// Maximum number of PTL retries before giving up and propagating the error.
pub const MAX_PTL_RETRIES: usize = 3;

/// Fallback drop fraction when no token-gap can be parsed from the error.
/// `1/FALLBACK_DROP_FRACTION` of groups are dropped.
const FALLBACK_DROP_FRACTION: usize = 5;

/// Marker text inserted as a synthetic User message when dropping the first
/// group (which may contain the system preamble) would leave the head of
/// the remaining slice as an Assistant message.
const PTL_RETRY_MARKER: &str = "[earlier conversation truncated for compaction retry]";

/// Group `messages` into segments each starting at a `Role::User` or
/// `Role::System` boundary. The first group always starts at index 0.
///
/// Returns the group boundaries as a `Vec<(usize, usize)>` of (start, end)
/// index ranges (end is exclusive, matching standard slice semantics).
pub fn group_by_user_boundary(messages: &[Message]) -> Vec<(usize, usize)> {
    if messages.is_empty() {
        return Vec::new();
    }

    let mut groups: Vec<(usize, usize)> = Vec::new();
    let mut start = 0_usize;

    for (i, msg) in messages.iter().enumerate().skip(1) {
        let is_boundary = msg.role == Role::User || msg.role == Role::System;
        if is_boundary {
            groups.push((start, i));
            start = i;
        }
    }

    // Push the final group.
    if start < messages.len() {
        groups.push((start, messages.len()));
    }

    groups
}

/// Estimate number of characters in a range of messages.
fn estimate_chars_range(messages: &[Message], range: (usize, usize)) -> usize {
    messages[range.0..range.1]
        .iter()
        .map(|m| m.content.len())
        .sum()
}

/// Drop the oldest groups from a to-summarise slice so the remaining
/// slice fits within a target character budget, or by a fallback fraction.
///
/// Never drops all groups — at least one group is always kept. Returns
/// `None` when there are fewer than 2 groups (nothing to drop), or when
/// the result after truncation would be empty after retreating past
/// orphaned Tool/Assistant-with-tool_calls messages.
///
/// `target_chars`:
/// - `Some(t)`: drop oldest groups until the remaining slice's estimated
///   chars are ≤ `t` (or as close as possible by dropping groups).
/// - `None`: drop `max(1, n_groups / FALLBACK_DROP_FRACTION)` oldest groups.
///
/// After truncation, the result is run through a pairing-safe check:
/// orphaned `Role::Tool` messages and `Role::Assistant` messages with
/// `tool_calls` at the head are removed. If the resulting head would be
/// `Role::Assistant` (without tool_calls), a synthetic `Message::user`
/// marker is prepended.
pub fn truncate_head_for_retry(
    messages: &[Message],
    target_chars: Option<usize>,
) -> Option<Vec<Message>> {
    let groups = group_by_user_boundary(messages);
    if groups.len() < 2 {
        return None;
    }

    let total_chars: usize = groups
        .iter()
        .map(|g| estimate_chars_range(messages, *g))
        .sum();

    // Determine how many groups to drop.
    let drop_count = match target_chars {
        Some(target) => {
            if target >= total_chars {
                // Already under target — no truncation needed.
                return None;
            }
            let need_to_drop = total_chars - target;
            let mut cumulative = 0_usize;
            let mut dropped = 0_usize;
            for g in &groups {
                let g_chars = estimate_chars_range(messages, *g);
                if cumulative + g_chars > need_to_drop {
                    break;
                }
                cumulative += g_chars;
                dropped += 1;
            }
            dropped
        }
        None => {
            let raw = groups.len() / FALLBACK_DROP_FRACTION;
            if raw < 1 {
                1
            } else {
                raw
            }
        }
    };

    if drop_count == 0 {
        return None;
    }

    // Never drop all groups — keep at least one.
    let keep_from = if drop_count >= groups.len() {
        groups.len() - 1
    } else {
        drop_count
    };

    // Build the result: flatten groups[keep_from..].
    let mut result: Vec<Message> = messages[groups[keep_from].0..].to_vec();

    // Retreat past orphaned Tool and Assistant-with-tool_calls at head.
    while !result.is_empty() {
        let head = &result[0];
        if head.role == Role::Tool || (head.role == Role::Assistant && !head.tool_calls.is_empty())
        {
            result.remove(0);
        } else {
            break;
        }
    }

    // If head is now Assistant (without tool_calls), prepend user marker.
    if result.first().map(|m| m.role) == Some(Role::Assistant) {
        result.insert(0, Message::user(PTL_RETRY_MARKER));
    }

    // If nothing remains after retreat, return None.
    if result.is_empty() {
        return None;
    }

    Some(result)
}

/// Best-effort token-gap parsing from an LLM error message.
///
/// Tries to extract a total token count from the error (e.g. `"resulted in
/// 17000 tokens"`) and returns a target char estimate (80% of the token
/// count converted via the heuristic 4 chars/token). Returns `None` when
/// nothing parseable is found — caller should use the fallback drop fraction.
pub fn estimate_target_from_error(err: &crate::error::Error) -> Option<usize> {
    let message = match err {
        crate::error::Error::Llm { message, .. } => message,
        _ => return None,
    };

    // Try to find patterns like "resulted in 12345 tokens" or "12345 tokens"
    let lower = message.to_lowercase();

    // Pattern 1: "resulted in <number> tokens"
    if let Some(tokens) = extract_number_after(&lower, "resulted in") {
        return Some((tokens * 4 * 80) / 100); // 80% of estimate
    }

    // Pattern 2: "<number> tokens" (before "tokens")
    if let Some(tokens) = extract_number_before_word(&lower, "tokens") {
        return Some((tokens * 4 * 80) / 100);
    }

    None
}

/// Extract a number from `text` that appears after the given `prefix`.
fn extract_number_after(text: &str, prefix: &str) -> Option<usize> {
    text.find(prefix).and_then(|idx| {
        let after = &text[idx + prefix.len()..];
        // Find the first contiguous digit sequence.
        let start = after.find(|c: char| c.is_ascii_digit())?;
        let end = after[start..]
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after[start..].len());
        after[start..start + end].parse::<usize>().ok()
    })
}

/// Extract a number from `text` that appears immediately before `word`.
fn extract_number_before_word(text: &str, word: &str) -> Option<usize> {
    text.find(word).and_then(|idx| {
        let before = &text[..idx];
        // Find the last contiguous digit sequence before the word.
        let end = before.len();
        let digit_start = before[..end].rfind(|c: char| c.is_ascii_digit())?;
        let word_start = before[..=digit_start]
            .rfind(|c: char| !c.is_ascii_digit())
            .map(|i| i + 1)
            .unwrap_or(0);
        before[word_start..=digit_start].parse::<usize>().ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolCall;
    use crate::message::Message;

    // ── group_by_user_boundary ────────────────────────────────────────

    #[test]
    fn group_by_user_boundary_empty_input() {
        let groups = group_by_user_boundary(&[]);
        assert!(groups.is_empty());
    }

    #[test]
    fn group_by_user_boundary_single_group() {
        // All assistant/tool messages (no User/System boundaries) → 1 group
        let msgs = vec![
            Message::assistant("a1".to_string()),
            Message::tool_result("c1", "result".to_string()),
        ];
        let groups = group_by_user_boundary(&msgs);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0], (0, 2));
    }

    #[test]
    fn group_by_user_boundary_isolated_assistant_no_user() {
        // No User or System boundaries after the first message → single group
        let msgs = vec![
            Message::assistant("a".to_string()),
            Message::assistant("b".to_string()),
        ];
        let groups = group_by_user_boundary(&msgs);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0], (0, 2));
    }

    #[test]
    fn group_by_user_boundary_splits_on_user_messages() {
        let msgs = vec![
            Message::system("sys".to_string()),
            Message::user("goal 1".to_string()),
            Message::assistant("reply 1".to_string()),
            Message::user("goal 2".to_string()),
            Message::assistant("reply 2".to_string()),
            Message::user("goal 3".to_string()),
            Message::assistant("reply 3".to_string()),
        ];
        let groups = group_by_user_boundary(&msgs);
        // Groups: [0..1) sys, [1..3) goal1+reply1, [3..5) goal2+reply2, [5..7) goal3+reply3
        assert_eq!(groups.len(), 4);
        assert_eq!(groups[0], (0, 1));
        assert_eq!(groups[1], (1, 3));
        assert_eq!(groups[2], (3, 5));
        assert_eq!(groups[3], (5, 7));
    }

    #[test]
    fn group_by_user_boundary_system_is_boundary() {
        // System message at index 3 starts a new group, User at 4 starts another
        let msgs = vec![
            Message::system("sys1".to_string()),
            Message::user("u1".to_string()),
            Message::assistant("a1".to_string()),
            Message::system("sys2".to_string()),
            Message::user("u2".to_string()),
        ];
        let groups = group_by_user_boundary(&msgs);
        // Boundaries at indices 1 (user), 3 (system), 4 (user) → 4 groups
        assert_eq!(groups.len(), 4);
        assert_eq!(groups[0], (0, 1)); // sys1
        assert_eq!(groups[1], (1, 3)); // u1+a1
        assert_eq!(groups[2], (3, 4)); // sys2
        assert_eq!(groups[3], (4, 5)); // u2
    }

    // ── truncate_head_for_retry ───────────────────────────────────────

    fn make_tool_call_asst(id: &str, content: &str) -> Message {
        Message::assistant_with_tool_calls(
            content.to_string(),
            vec![ToolCall {
                id: id.into(),
                name: "Read".into(),
                arguments: serde_json::json!({}),
            }],
        )
    }

    #[test]
    fn truncate_head_returns_none_when_one_group() {
        // Only assistant messages → single group → no truncation needed
        let msgs = vec![
            Message::assistant("only msg".to_string()),
            Message::assistant("another".to_string()),
        ];
        let result = truncate_head_for_retry(&msgs, Some(10));
        assert!(result.is_none(), "should return None with only 1 group");
    }

    #[test]
    fn truncate_head_drops_oldest_groups() {
        // 3 groups, target drops 1 group
        let msgs = vec![
            Message::system("s".to_string()),        // group 0, 1 char
            Message::user("abc".to_string()),        // group 1, 3 chars
            Message::assistant("def".to_string()),   // part of group 1
            Message::user("ghij".to_string()),       // group 2, 4 chars
            Message::assistant("klmno".to_string()), // part of group 2
        ];
        // groups: [0..1), [1..3), [3..5)
        // Total = 1+3+3+4+5 = 16 chars
        // target=15 → need to drop 1 char. Group 0 = 1 char ≥ 1 → drop 1 group
        let result = truncate_head_for_retry(&msgs, Some(15));
        assert!(result.is_some(), "should return truncated messages");
        let result = result.unwrap();
        // Remaining: groups[1..] = [user("abc"), asst("def"), user("ghij"), asst("klmno")]
        assert_eq!(result.len(), 4);
        assert_eq!(result[0].content, "abc");
        assert_eq!(result[0].role, Role::User);
    }

    #[test]
    fn truncate_head_keeps_at_least_one_group() {
        // Target would drop all → keeps the last group.
        let msgs = vec![
            Message::user("group1".to_string()),
            Message::assistant("a".to_string()),
            Message::user("group2".to_string()),
            Message::assistant("b".to_string()),
        ];
        // 2 groups, target=0 → keep at least 1 group
        let result = truncate_head_for_retry(&msgs, Some(0));
        assert!(result.is_some(), "should keep at least one group");
        let result = result.unwrap();
        // Should have kept the last group (group2)
        assert_eq!(result[0].content, "group2");
    }

    #[test]
    fn truncate_head_preserves_pairing() {
        // Input with a tool-call pair spanning a group boundary after
        // dropping oldest group.
        let msgs = vec![
            Message::system("sys".to_string()),          // group 0
            Message::user("old".to_string()),            // group 0
            Message::assistant("old reply".to_string()), // group 0
            make_tool_call_asst("c1", "reading file"),   // group 1 — Asst+tc
            Message::tool_result("c1", "contents"),      // group 1
            Message::user("next".to_string()),           // group 2
            Message::assistant("done".to_string()),      // group 2
        ];
        // groups: [0..3), [3..5), [5..7)
        // Drop group 0 → remaining = [Asst+tc, Tool, User, Asst]
        // Retreat past Asst+tc → [Tool, User, Asst]
        // Retreat past Tool → [User, Asst]
        let result = truncate_head_for_retry(&msgs, None);
        assert!(
            result.is_some(),
            "should handle tool-call spanning boundary"
        );
        let result = result.unwrap();
        // Head must not be orphan Tool or Asst+tc
        assert!(result[0].role != Role::Tool, "head must not be orphan Tool");
        assert!(
            result[0].role != Role::Assistant || result[0].tool_calls.is_empty(),
            "head must not be orphan Asst+tc"
        );
    }

    #[test]
    fn truncate_head_fallback_drop_fraction() {
        // 6 groups → 6/5 = 1 → drop 1 group.
        let mut msgs: Vec<Message> = Vec::new();
        for i in 0..6 {
            msgs.push(Message::user(format!("g{i}")));
            msgs.push(Message::assistant(format!("r{i}")));
        }
        let groups = group_by_user_boundary(&msgs);
        assert_eq!(groups.len(), 6);
        let result = truncate_head_for_retry(&msgs, None);
        assert!(result.is_some(), "should drop oldest groups via fallback");
        let result = result.unwrap();
        // Should have dropped 1 group (5 remaining = 10 msgs)
        assert_eq!(result.len(), 10);
        assert_eq!(result[0].content, "g1");
    }

    #[test]
    fn truncate_head_empty_input_returns_none() {
        let result = truncate_head_for_retry(&[], Some(100));
        assert!(result.is_none(), "empty input should return None");
    }

    #[test]
    fn truncate_head_target_too_high_returns_none() {
        // target >= total_chars → already under target
        let msgs = vec![
            Message::user("ab".to_string()),
            Message::assistant("cd".to_string()),
            Message::user("ef".to_string()),
            Message::assistant("gh".to_string()),
        ];
        let result = truncate_head_for_retry(&msgs, Some(100));
        assert!(result.is_none(), "target >= total should return None");
    }

    // ── estimate_target_from_error ────────────────────────────────────

    #[test]
    fn estimate_target_extracts_resulted_in_tokens() {
        let err = crate::error::Error::Llm {
            provider: "openai".into(),
            message: "This model's maximum context length is 16385 tokens. \
                       However, your messages resulted in 17000 tokens."
                .into(),
        };
        let target = estimate_target_from_error(&err);
        assert!(target.is_some(), "should parse token count from error");
        // 17000 * 4 * 0.8 = 54400
        assert_eq!(target.unwrap(), 54400);
    }

    #[test]
    fn estimate_target_extracts_basic_token_count() {
        let err = crate::error::Error::Llm {
            provider: "deepseek".into(),
            message: "prompt is too long (16000 tokens)".into(),
        };
        let target = estimate_target_from_error(&err);
        assert!(target.is_some(), "should parse token count");
        // 16000 * 4 * 0.8 = 51200
        assert_eq!(target.unwrap(), 51200);
    }

    #[test]
    fn estimate_target_returns_none_for_non_llm_error() {
        let err = crate::error::Error::Config {
            message: "something".into(),
        };
        let target = estimate_target_from_error(&err);
        assert!(target.is_none(), "non-LLM errors should return None");
    }

    #[test]
    fn estimate_target_returns_none_for_unparseable_message() {
        let err = crate::error::Error::Llm {
            provider: "test".into(),
            message: "some other error without numbers".into(),
        };
        let target = estimate_target_from_error(&err);
        assert!(target.is_none(), "unparseable messages should return None");
    }

    #[test]
    fn estimate_target_parses_tokens_in_context_window_format() {
        let err = crate::error::Error::Llm {
            provider: "test".into(),
            message: "request exceeds the model context window of 128000 tokens".into(),
        };
        let target = estimate_target_from_error(&err);
        assert!(target.is_some(), "should parse token count");
        // 128000 * 4 * 0.8 = 409600
        assert_eq!(target.unwrap(), 409600);
    }
}
