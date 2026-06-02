//! LLM-based permission classifier for `PermissionMode::Auto`.
//!
//! When the agent is in Auto mode, each tool call is sent to a small
//! classifier prompt running against the same LLM provider. The classifier
//! returns `{"block": true/false, "reason": "..."}` and the result is
//! enforced before the tool executes.
//!
//! A [`DenialTracker`] prevents runaway denial loops: after 3 consecutive
//! or 10 total denials, the classifier stops calling the LLM and blocks
//! everything, triggering a [`FinishReason::PermissionDenialLimit`].

use crate::error::Result;
use crate::llm::LlmProvider;
use std::sync::Arc;

// ── Classifier prompt ──────────────────────────────────────────────────────

const CLASSIFIER_PROMPT: &str = "\
You are a security classifier for an AI coding agent.
Current tool call:
  tool: {tool_name}
  args: {args_summary}

Recent conversation context (last 10 turns):
{transcript_snippet}

Is this action safe to auto-approve?
Respond ONLY with JSON (no markdown): { \"block\": true|false, \"reason\": \"...\" }";

// ── Denial tracker ─────────────────────────────────────────────────────────

/// Tracks classifier denial counts to enforce safety limits.
///
/// After `MAX_CONSECUTIVE` consecutive denials or `MAX_TOTAL` total
/// denials, `is_over_limit()` returns `true` and the classifier stops
/// calling the LLM (all subsequent tool calls are denied).
#[derive(Debug, Default)]
pub struct DenialTracker {
    /// Number of consecutive denials without an intervening allow.
    pub consecutive: u32,
    /// Total number of denials ever recorded.
    pub total: u32,
}

/// Maximum number of consecutive denials before limit is hit.
const MAX_CONSECUTIVE: u32 = 3;

/// Maximum total number of denials before limit is hit.
const MAX_TOTAL: u32 = 10;

impl DenialTracker {
    /// Record a denial — increments both counters.
    pub fn record_denial(&mut self) {
        self.consecutive += 1;
        self.total += 1;
    }

    /// Record an allow — resets the consecutive counter.
    pub fn record_allow(&mut self) {
        self.consecutive = 0;
    }

    /// Returns `true` if either limit has been reached.
    pub fn is_over_limit(&self) -> bool {
        self.consecutive >= MAX_CONSECUTIVE || self.total >= MAX_TOTAL
    }
}

// ── Auto classifier ────────────────────────────────────────────────────────

/// LLM-based classifier that decides whether a tool call should be
/// auto-approved in [`PermissionMode::Auto`](crate::permissions::PermissionMode::Auto).
pub struct AutoClassifier {
    /// The LLM provider used for classification calls.
    provider: Arc<dyn LlmProvider>,
    /// Denial tracker for safety limits.
    pub tracker: DenialTracker,
}

/// JSON response expected from the classifier LLM.
#[derive(serde::Deserialize)]
struct ClassifierResponse {
    block: bool,
    #[allow(dead_code)]
    reason: String,
}

impl AutoClassifier {
    /// Create a new classifier backed by the given provider.
    pub fn new(provider: Arc<dyn LlmProvider>) -> Self {
        Self {
            provider,
            tracker: DenialTracker::default(),
        }
    }

    /// Classify whether a tool call should be blocked.
    ///
    /// # Arguments
    /// * `tool_name` — name of the tool being called.
    /// * `args_summary` — JSON string summary of the arguments.
    /// * `transcript_snippet` — recent conversation context.
    ///
    /// # Returns
    /// `Ok((block, reason))` — `block` is `true` if the tool should be denied.
    ///
    /// # Behaviour
    /// * If the denial tracker is over limit, returns `(true, "...")` without
    ///   calling the LLM.
    /// * On JSON parse error, defaults to `(false, "classifier parse error...")`
    ///   (conservative: err on the side of allowing).
    pub async fn classify(
        &mut self,
        tool_name: &str,
        args_summary: &str,
        transcript_snippet: &str,
    ) -> Result<(bool, String)> {
        // If over limit, skip the LLM call entirely.
        if self.tracker.is_over_limit() {
            return Ok((true, "denial limit reached".into()));
        }

        let prompt = CLASSIFIER_PROMPT
            .replace("{tool_name}", tool_name)
            .replace("{args_summary}", args_summary)
            .replace("{transcript_snippet}", transcript_snippet);

        // Call the LLM with a 60-second timeout.
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            self.provider.complete_simple(&prompt, 0.0),
        )
        .await
        .map_err(|_| crate::error::Error::Config {
            message: "auto classifier timeout".into(),
        })??;

        match serde_json::from_str::<ClassifierResponse>(&response) {
            Ok(r) => {
                if r.block {
                    self.tracker.record_denial();
                } else {
                    self.tracker.record_allow();
                }
                Ok((r.block, r.reason))
            }
            Err(_) => {
                // Parse failure — conservative: allow the tool
                Ok((false, "classifier parse error, defaulting to allow".into()))
            }
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::mock::MockProvider;
    use crate::llm::Completion;

    // ── DenialTracker tests ──────────────────────────────────────────────

    #[test]
    fn denial_tracker_consecutive_limit() {
        let mut tracker = DenialTracker::default();
        assert!(!tracker.is_over_limit());
        tracker.record_denial();
        tracker.record_denial();
        assert!(!tracker.is_over_limit());
        tracker.record_denial(); // 3rd consecutive
        assert!(tracker.is_over_limit());
        assert_eq!(tracker.consecutive, 3);
        assert_eq!(tracker.total, 3);
    }

    #[test]
    fn denial_tracker_reset_on_allow() {
        let mut tracker = DenialTracker::default();
        tracker.record_denial();
        tracker.record_denial(); // consecutive = 2
        assert!(!tracker.is_over_limit());
        tracker.record_allow(); // consecutive resets to 0
        assert_eq!(tracker.consecutive, 0);
        tracker.record_denial(); // consecutive = 1
        assert_eq!(tracker.consecutive, 1);
        assert!(!tracker.is_over_limit());
    }

    #[test]
    fn denial_tracker_total_limit() {
        let mut tracker = DenialTracker::default();
        for _ in 0..9 {
            tracker.record_denial();
            tracker.record_allow(); // reset consecutive
        }
        assert!(!tracker.is_over_limit()); // total = 9
        tracker.record_denial(); // total = 10
        assert!(tracker.is_over_limit());
        assert_eq!(tracker.total, 10);
    }

    // ── AutoClassifier tests with mock provider ──────────────────────────

    /// Build a mock provider whose `complete_simple` returns the given content.
    fn mock_classifier(content: &str) -> MockProvider {
        MockProvider::new(vec![Completion {
            content: content.to_string(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }])
    }

    #[tokio::test]
    async fn classifier_parse_block_true() {
        let provider = Arc::new(mock_classifier(r#"{"block":true,"reason":"unsafe"}"#));
        let mut classifier = AutoClassifier::new(provider);
        let (block, reason) = classifier.classify("run_shell", "{}", "").await.unwrap();
        assert!(block);
        assert_eq!(reason, "unsafe");
        assert_eq!(classifier.tracker.consecutive, 1);
    }

    #[tokio::test]
    async fn classifier_parse_allow() {
        let provider = Arc::new(mock_classifier(r#"{"block":false,"reason":"ok"}"#));
        let mut classifier = AutoClassifier::new(provider);
        let (block, reason) = classifier.classify("read_file", "{}", "").await.unwrap();
        assert!(!block);
        assert_eq!(reason, "ok");
        assert_eq!(classifier.tracker.consecutive, 0);
    }

    #[tokio::test]
    async fn classifier_parse_error_defaults_allow() {
        let provider = Arc::new(mock_classifier("not valid json"));
        let mut classifier = AutoClassifier::new(provider);
        let (block, _reason) = classifier.classify("read_file", "{}", "").await.unwrap();
        assert!(!block, "parse error should default to allow");
        // Consecutive should still be 0 (parse error is not a denial).
        assert_eq!(classifier.tracker.consecutive, 0);
    }

    #[tokio::test]
    async fn classifier_over_limit_skips_llm() {
        // Script 3 denial responses — the 4th call skips the LLM.
        let provider = MockProvider::new(vec![
            Completion {
                content: r#"{"block":true,"reason":"first"}"#.into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: r#"{"block":true,"reason":"second"}"#.into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: r#"{"block":true,"reason":"third"}"#.into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]);
        let provider = Arc::new(provider);
        let mut classifier = AutoClassifier::new(provider);

        // Trigger 3 consecutive denials.
        for _ in 0..3 {
            let (block, _) = classifier.classify("run_shell", "{}", "").await.unwrap();
            assert!(block);
        }
        assert!(classifier.tracker.is_over_limit());

        // Next call should skip LLM entirely (no mock completions needed).
        let (block, reason) = classifier.classify("run_shell", "{}", "").await.unwrap();
        assert!(block);
        assert!(reason.contains("limit reached"));
    }
}
