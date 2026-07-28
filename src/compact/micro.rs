//! No-LLM proactive pruning of old tool results by count.
//!
//! Runs before [`crate::compact::Compactor`] (LLM-driven compaction) so that
//! after pruning tool results by count, the transcript may drop below the
//! char-based compaction threshold and the expensive LLM summary is skipped
//! entirely. See [`Microcompactor::prune`] for details.

use crate::message::{Message, Role};

/// Placeholder reused from `run_core::TRIM_PLACEHOLDER` semantics.
/// Re-exported here so the microcompactor and the char-budget trim use the
/// same marker text.
pub const MICROCOMPACT_PLACEHOLDER: &str = "[older tool output trimmed to fit budget]";

/// Minimum tool-result size worth pruning; shorter results are kept verbatim
/// (matches `run_core::MIN_TRIM_LENGTH`).
pub const MIN_PRUNE_LENGTH: usize = 200;

/// Configuration for count-based proactive pruning of old tool results.
///
/// Unlike [`crate::compact::Compactor`] (LLM-driven), this compactor requires
/// no provider access — it simply replaces the `content` of old tool-result
/// messages with a placeholder string when the total number of tool messages
/// exceeds a threshold.
///
/// # Pairing safety (invariant #8)
///
/// This compactor **never removes messages** — only replaces the `content`
/// field of `Role::Tool` messages. The tool-call ↔ tool-result pairing is
/// preserved because every `Role::Tool` message stays at its index,
/// immediately after the `Role::Assistant` message whose `tool_calls` list
/// its `id`.
#[derive(Debug, Clone)]
pub struct Microcompactor {
    /// Prune when the number of `Role::Tool` messages exceeds this.
    pub trigger_tool_count: usize,
    /// Keep this many most-recent tool results verbatim.
    pub keep_recent: usize,
}

impl Default for Microcompactor {
    fn default() -> Self {
        Self {
            trigger_tool_count: 12,
            keep_recent: 4,
        }
    }
}

impl Microcompactor {
    /// Create a new `Microcompactor` with explicit trigger and keep counts.
    pub fn new(trigger_tool_count: usize, keep_recent: usize) -> Self {
        Self {
            trigger_tool_count,
            keep_recent,
        }
    }

    /// Prune oldest tool-result contents in place. Returns the number pruned.
    ///
    /// Does NOT remove messages — only replaces `content` of `Role::Tool`
    /// messages older than `keep_recent` when total tool count exceeds
    /// `trigger_tool_count`. Tool messages shorter than `MIN_PRUNE_LENGTH`
    /// are left untouched (not worth the placeholder swap).
    ///
    /// # Logic
    ///
    /// 1. Collect indices of all `Role::Tool` messages.
    /// 2. If `count <= self.trigger_tool_count`, return `0`.
    /// 3. Otherwise, the candidates for pruning are all tool messages
    ///    **except** the last `self.keep_recent`. Iterate them oldest-first;
    ///    for each whose `content.len() > MIN_PRUNE_LENGTH` and whose content
    ///    is not already the placeholder, replace `content` with
    ///    `MICROCOMPACT_PLACEHOLDER`. Stop once
    ///    `count - pruned <= self.trigger_tool_count` (prune only enough to
    ///    get back under the trigger — don't prune everything eligible).
    /// 4. Return the number pruned.
    pub fn prune(&self, messages: &mut [Message]) -> usize {
        // 1. Collect indices of all Role::Tool messages.
        let tool_indices: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == Role::Tool)
            .map(|(i, _)| i)
            .collect();

        let count = tool_indices.len();

        // 2. If count <= trigger, return 0.
        if count <= self.trigger_tool_count {
            return 0;
        }

        // 3. Candidates are all tool messages except the last `keep_recent`.
        //    The eligible indices are the oldest `count - keep_recent` ones.
        let eligible_count = count.saturating_sub(self.keep_recent);
        // We only need to prune enough to get back under the trigger.
        let target_prune = count.saturating_sub(self.trigger_tool_count);
        let to_process = eligible_count.min(target_prune);

        let mut pruned = 0;
        for idx in tool_indices.iter().take(to_process) {
            let idx = *idx;
            let msg = &mut messages[idx];
            // Skip if already placeholdered or too short.
            if msg.content == MICROCOMPACT_PLACEHOLDER || msg.content.len() <= MIN_PRUNE_LENGTH {
                continue;
            }
            msg.content = MICROCOMPACT_PLACEHOLDER.to_string();
            pruned += 1;
        }

        pruned
    }
}

/// Build a `Microcompactor` from environment-variable strings.
///
/// **Disabled by default.** Set `RECURSIVE_MICROCOMPACT_TRIGGER=<n>` (e.g.
/// `40`) to enable. Disabled when unset or when the value is `0`, `off`, or
/// `false`.
///
/// Rationale: the old default of 12 was too aggressive for large-context
/// models (1 M+). At 12 tool messages the pruner fires when the session is
/// only ~50–80 K tokens — 5–8 % of a 1 M window — causing surprising ctx
/// gauge drops. Opt-in lets operators tune the threshold to their model size.
///
/// `trigger_raw` — the value of `RECURSIVE_MICROCOMPACT_TRIGGER`.
/// `keep_raw`    — the value of `RECURSIVE_MICROCOMPACT_KEEP`.
pub fn build_microcompactor_from_env(
    trigger_raw: Option<&str>,
    keep_raw: Option<&str>,
) -> Option<Microcompactor> {
    let trigger: usize = match trigger_raw {
        None | Some("0") | Some("off") | Some("false") => return None, // disabled by default
        Some(s) => s.parse::<usize>().ok().filter(|&n| n > 0)?,
    };
    let keep: usize = match keep_raw {
        Some(s) => s.parse::<usize>().ok().unwrap_or(4),
        None => 4,
    };
    Some(Microcompactor::new(trigger, keep))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;

    /// Helper to build a tool message with a given content length.
    fn tool_msg(content_len: usize) -> Message {
        Message::tool_result("call_1", "x".repeat(content_len))
    }

    // ====================================================================
    // Core prune logic
    // ====================================================================

    #[test]
    fn prune_noop_when_under_trigger() {
        // 5 tool messages, trigger 12 → 0 pruned.
        let mut msgs: Vec<Message> = (0..5).map(|_| tool_msg(300)).collect();
        let mc = Microcompactor::new(12, 4);
        let pruned = mc.prune(&mut msgs);
        assert_eq!(pruned, 0);
        // All messages should still have their original content.
        for msg in &msgs {
            assert_ne!(msg.content, MICROCOMPACT_PLACEHOLDER);
        }
    }

    #[test]
    fn prune_oldest_keeps_recent() {
        // 15 tool messages, trigger 12, keep 4.
        // Total = 15, trigger = 12 → we need to prune 3 to get back to 12.
        // Eligible count = 15 - 4 = 11. Target = 15 - 12 = 3.
        // The 3 oldest (indices 0,1,2) should be pruned.
        let mut msgs: Vec<Message> = (0..15).map(|_| tool_msg(300)).collect();
        let mc = Microcompactor::new(12, 4);
        let pruned = mc.prune(&mut msgs);
        assert_eq!(
            pruned, 3,
            "should prune exactly 3 to get back under trigger"
        );

        // The last 4 should be untouched (indices 11..14 after sorting).
        for msg in msgs[11..15].iter() {
            assert_ne!(
                msg.content, MICROCOMPACT_PLACEHOLDER,
                "message must not be pruned"
            );
        }
        // The first 3 should be placeholdered (indices 0,1,2).
        for msg in msgs[0..3].iter() {
            assert_eq!(
                msg.content, MICROCOMPACT_PLACEHOLDER,
                "message must be pruned"
            );
        }
        // Messages 3..10 should still be untouched (not placeholdered)
        // because we stop pruning once count - pruned <= trigger.
        for msg in msgs[3..11].iter() {
            assert_ne!(
                msg.content, MICROCOMPACT_PLACEHOLDER,
                "message should not be pruned (pruning stopped at 3)"
            );
        }
    }

    #[test]
    fn prune_preserves_tool_messages() {
        // After pruning, the message count is unchanged and every pruned
        // index is still Role::Tool (pairing intact).
        let mut msgs: Vec<Message> = (0..15).map(|_| tool_msg(300)).collect();
        let total_before = msgs.len();
        let mc = Microcompactor::new(12, 4);
        let _pruned = mc.prune(&mut msgs);
        assert_eq!(msgs.len(), total_before, "message count must not change");
        for msg in &msgs {
            assert_eq!(msg.role, Role::Tool, "all messages must still be Tool role");
        }
    }

    #[test]
    fn prune_skips_short_results() {
        // A tool result under MIN_PRUNE_LENGTH is not placeholdered even if
        // it's old. Create 15 tool messages where the oldest are short.
        let mut msgs: Vec<Message> = Vec::new();
        // First 3 are short (under MIN_PRUNE_LENGTH).
        for _ in 0..3 {
            msgs.push(tool_msg(50));
        }
        // Next 8 are long enough.
        for _ in 0..8 {
            msgs.push(tool_msg(300));
        }
        // Last 4 are long (recent, will be kept).
        for _ in 0..4 {
            msgs.push(tool_msg(300));
        }

        let mc = Microcompactor::new(12, 4);
        let pruned = mc.prune(&mut msgs);
        // Eligible = 15 - 4 = 11, target = 15 - 12 = 3.
        // The first 3 candidates are short (50 bytes each) → skipped.
        // So we should prune 0 (or up to 3 from later ones if we iterate further).
        // But the goal says: iterate oldest-first; skip short ones.
        // With to_process = 3, indices 0,1,2 are all short → 0 pruned.
        assert_eq!(pruned, 0, "short results should not be pruned");
    }

    #[test]
    fn prune_skips_short_results_and_prunes_eligible_ones() {
        // Mix of short and long: the short ones should be skipped, and only
        // the old long ones should be pruned until the count is under trigger.
        let mut msgs: Vec<Message> = Vec::new();
        // First 2: short (skipped).
        msgs.push(tool_msg(50));
        msgs.push(tool_msg(50));
        // Next 5: long (eligible).
        for _ in 0..5 {
            msgs.push(tool_msg(300));
        }
        // Last 4: long (recent, kept).
        for _ in 0..4 {
            msgs.push(tool_msg(300));
        }
        // Total = 11, trigger = 8, keep = 4.
        // Eligible = 11 - 4 = 7, target = 11 - 8 = 3.
        // to_process = min(7, 3) = 3.
        // Candidates: indices 0 (short, skip), 1 (short, skip), 2 (long, prune ✓).
        // So we try to prune 3, but only the long one gets pruned, count = 1.
        let mc = Microcompactor::new(8, 4);
        let pruned = mc.prune(&mut msgs);

        // First 2 (short) should be untouched.
        assert_ne!(msgs[0].content, MICROCOMPACT_PLACEHOLDER);
        assert_ne!(msgs[1].content, MICROCOMPACT_PLACEHOLDER);
        // Index 2 (long, old) should be pruned.
        assert_eq!(msgs[2].content, MICROCOMPACT_PLACEHOLDER);
        // Recent (last 4) should be untouched.
        for msg in msgs[7..11].iter() {
            assert_ne!(msg.content, MICROCOMPACT_PLACEHOLDER);
        }
        // We pruned at least 1 (the old long one).
        assert!(pruned >= 1, "should prune at least 1 eligible long result");
    }

    #[test]
    fn prune_idempotent() {
        // Running prune twice on the same transcript does nothing the second
        // time (already-placeholdered content is skipped).
        let mut msgs: Vec<Message> = (0..15).map(|_| tool_msg(300)).collect();
        let mc = Microcompactor::new(12, 4);
        let first = mc.prune(&mut msgs);
        assert_eq!(first, 3, "first prune should prune 3");

        let second = mc.prune(&mut msgs);
        assert_eq!(second, 0, "second prune should be no-op (idempotent)");
    }

    #[test]
    fn prune_exact_at_trigger_no_prune() {
        // Exactly at trigger count → no pruning.
        let mut msgs: Vec<Message> = (0..12).map(|_| tool_msg(300)).collect();
        let mc = Microcompactor::new(12, 4);
        let pruned = mc.prune(&mut msgs);
        assert_eq!(pruned, 0);
    }

    #[test]
    fn prune_just_over_trigger_prunes_appropriate_number() {
        // 13 tool messages, trigger 12, keep 4.
        // count = 13 > 12 → prune.
        // eligible = 13 - 4 = 9, target = 13 - 12 = 1.
        // to_process = min(9, 1) = 1.
        // Only 1 message should be pruned.
        let mut msgs: Vec<Message> = (0..13).map(|_| tool_msg(300)).collect();
        let mc = Microcompactor::new(12, 4);
        let pruned = mc.prune(&mut msgs);
        assert_eq!(pruned, 1, "13 > 12 → should prune exactly 1");
    }

    // ====================================================================
    // build_microcompactor_from_env tests
    // ====================================================================

    #[test]
    fn build_microcompactor_from_env_disabled_when_zero() {
        // `0` → disabled (None).
        assert!(build_microcompactor_from_env(Some("0"), None).is_none());
        assert!(build_microcompactor_from_env(Some("off"), None).is_none());
        assert!(build_microcompactor_from_env(Some("false"), None).is_none());
    }

    #[test]
    fn build_microcompactor_from_env_disabled_by_default() {
        // Microcompactor is OFF when env var is unset — opt-in only.
        assert!(
            build_microcompactor_from_env(None, None).is_none(),
            "unset trigger must return None (disabled by default)"
        );
    }

    #[test]
    fn build_microcompactor_from_env_explicit_values() {
        let mc = build_microcompactor_from_env(Some("20"), Some("6")).unwrap();
        assert_eq!(mc.trigger_tool_count, 20);
        assert_eq!(mc.keep_recent, 6);
    }

    #[test]
    fn build_microcompactor_from_env_trigger_only() {
        // keep unset → default 4.
        let mc = build_microcompactor_from_env(Some("15"), None).unwrap();
        assert_eq!(mc.trigger_tool_count, 15);
        assert_eq!(mc.keep_recent, 4);
    }

    #[test]
    fn build_microcompactor_from_env_invalid_trigger_returns_none() {
        // Non-numeric trigger cannot be parsed → None (disabled), not fallback.
        assert!(
            build_microcompactor_from_env(Some("not-a-number"), None).is_none(),
            "unparseable trigger must return None"
        );
    }
}
