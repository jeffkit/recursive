//! Ordered output ledger for `run_code` (issue #134, borrowed from DSH
//! `ptc-runtime/output-ledger.ts`).
//!
//! The output budget covers the *serialized* run: every log line as it is
//! produced, plus the program's completion value. Log lines go through the
//! ledger; the completion value is bounded by [`OutputLedger::remaining`] via
//! [`truncate_to_budget`], so both parts share ONE budget and the rendered
//! observation stays within it. When the budget is reached the ledger keeps
//! the largest prefix that still fits and marks itself truncated — the run is
//! reported as `output-limit` instead of throwing the partial transcript
//! away.
//!
//! Truncation is UTF-8 safe: the kept prefix always ends on a char boundary,
//! so the ledger never emits a broken code point.

/// Keep the largest UTF-8-safe prefix of `text` that fits `budget`, returning
/// it and whether anything was dropped.
///
/// The counterpart of [`OutputLedger::append`] for a fragment the caller
/// renders *separately* from the ledger (the program's completion value): it
/// is still bounded by the same budget, but the caller needs the kept text
/// back rather than appended.
pub fn truncate_to_budget(text: &str, budget: usize) -> (&str, bool) {
    if text.len() <= budget {
        return (text, false);
    }
    let mut end = budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

/// A bounded, append-only byte buffer that remembers whether it dropped
/// anything.
#[derive(Debug, Clone)]
pub struct OutputLedger {
    limit: usize,
    written: Vec<u8>,
    truncated: bool,
}

impl OutputLedger {
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            written: Vec::new(),
            truncated: false,
        }
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Bytes currently retained.
    pub fn len(&self) -> usize {
        self.written.len()
    }

    pub fn is_empty(&self) -> bool {
        self.written.is_empty()
    }

    /// Budget still available. The completion value is bounded by this (see
    /// [`truncate_to_budget`]) so log lines and the value share ONE budget
    /// instead of each getting the full limit.
    pub fn remaining(&self) -> usize {
        self.limit.saturating_sub(self.written.len())
    }

    /// Nothing was dropped.
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// Record that a fragment was dropped *before* it reached the ledger —
    /// the reader discards a single oversized protocol line rather than
    /// buffering it (see `runner::read_capped_line`). The ledger stays the
    /// single source of truth for "the output budget was exceeded".
    pub fn mark_truncated(&mut self) {
        self.truncated = true;
    }

    /// Append `fragment`, keeping at most the remaining budget. Any overflow is
    /// dropped (the prefix that fits is preserved) and flips
    /// [`Self::is_truncated`].
    pub fn append(&mut self, fragment: &str) {
        let (kept, dropped) = truncate_to_budget(fragment, self.remaining());
        self.written.extend_from_slice(kept.as_bytes());
        self.truncated |= dropped;
    }

    /// The retained prefix. Always valid UTF-8 by construction.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.written).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragments_within_budget_are_kept_in_order() {
        let mut ledger = OutputLedger::new(64);
        ledger.append("one\n");
        ledger.append("two\n");
        assert_eq!(ledger.text(), "one\ntwo\n");
        assert_eq!(ledger.len(), 8);
        assert!(!ledger.is_truncated());
    }

    #[test]
    fn overflow_keeps_the_largest_prefix() {
        let mut ledger = OutputLedger::new(5);
        ledger.append("abcde");
        ledger.append("fghij");
        assert_eq!(ledger.text(), "abcde");
        assert!(ledger.is_truncated());
        assert_eq!(ledger.len(), 5);
    }

    /// A fragment that straddles the limit keeps only the part that fits.
    #[test]
    fn straddling_fragment_keeps_its_head() {
        let mut ledger = OutputLedger::new(7);
        ledger.append("abc");
        ledger.append("defghij");
        assert_eq!(ledger.text(), "abcdefg");
        assert!(ledger.is_truncated());
    }

    /// The cut never splits a multi-byte char: with a budget of 4 bytes and a
    /// fragment that would need 6, only the two 2-byte chars fit.
    #[test]
    fn truncation_is_utf8_safe() {
        let mut ledger = OutputLedger::new(4);
        ledger.append("éé"); // two 2-byte chars, exactly 4 bytes
        ledger.append("é"); // 2 more bytes — none fit
        assert_eq!(ledger.text(), "éé");
        assert!(ledger.is_truncated());

        let mut narrow = OutputLedger::new(5);
        narrow.append("aé"); // 1 + 2 = 3 bytes
        narrow.append("bbb"); // 2 more bytes fit ("bb"), the third does not
        assert_eq!(narrow.text(), "aébb");
        assert!(narrow.is_truncated());
        // The result is valid UTF-8 regardless.
        assert_eq!(narrow.text().chars().count(), 4);
    }

    #[test]
    fn zero_budget_truncates_everything() {
        let mut ledger = OutputLedger::new(0);
        ledger.append("anything");
        assert!(ledger.is_empty());
        assert!(ledger.is_truncated());
    }

    #[test]
    fn appends_after_saturation_are_dropped() {
        let mut ledger = OutputLedger::new(3);
        ledger.append("abc");
        ledger.append("def");
        ledger.append("ghi");
        assert_eq!(ledger.text(), "abc");
        assert!(ledger.is_truncated());
    }

    #[test]
    fn truncate_to_budget_keeps_a_utf8_safe_prefix() {
        assert_eq!(truncate_to_budget("abc", 3), ("abc", false));
        assert_eq!(truncate_to_budget("abc", 10), ("abc", false));
        assert_eq!(truncate_to_budget("abcdef", 3), ("abc", true));
        // Never split a multi-byte char — "aébbb" is 6 bytes, so a 4-byte
        // budget keeps "aé" + "b".
        assert_eq!(truncate_to_budget("aébbb", 4), ("aéb", true));
        assert_eq!(truncate_to_budget("é", 1), ("", true));
        assert_eq!(truncate_to_budget("", 0), ("", false));
    }

    #[test]
    fn remaining_never_underflows_at_the_limit() {
        let mut ledger = OutputLedger::new(3);
        assert_eq!(ledger.remaining(), 3);
        ledger.append("abcd");
        assert_eq!(ledger.remaining(), 0);
        ledger.append("x");
        assert_eq!(ledger.remaining(), 0);
    }

    /// A fragment dropped by the reader (before it ever reached the ledger)
    /// still has to be reported as truncation.
    #[test]
    fn mark_truncated_flags_a_dropped_fragment() {
        let mut ledger = OutputLedger::new(64);
        assert!(!ledger.is_truncated());
        ledger.mark_truncated();
        assert!(ledger.is_truncated());
        assert!(ledger.is_empty());
    }
}
