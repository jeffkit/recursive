//! Issue #114 — per-session usage & cost accounting for the HTTP surface.
//!
//! Before this module the HTTP channel held only two in-memory atomics
//! (`prompt_tokens` / `completion_tokens`): no cache split, no USD, and
//! nothing persisted. `cost.json` / `.meta.json` existed on the CLI and
//! AG-UI paths only, so a server restart zeroed every session's counters
//! (`cold_load` re-initialised them to 0 with nothing to restore from) and
//! no HTTP client ever saw a dollar figure.
//!
//! This module gives the HTTP channel the same accounting the other channels
//! use: the token split comes straight from the provider
//! ([`crate::llm::TokenUsage`]) and USD from [`crate::llm::pricing_for`] +
//! [`ModelPricing::cost_usd`](crate::llm::ModelPricing::cost_usd) — the exact
//! functions `CostTracker` bills with — accumulated lock-free on
//! [`SessionUsage`] and snapshotted to [`UsageTotals`]. The snapshot is
//! written to the storage backends' generic key/value space
//! (`session-usage/<id>`, the same space as the session metadata blob), so it
//! works for every backend (local / S3) and outlives a restart.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::llm::TokenUsage;
use crate::storage::StorageBackend;

/// USD cost of one turn at `model`, rounded to micro-USD.
///
/// Zero for a model with no pricing entry — the same "unpriced contributes
/// nothing" rule [`super::Metrics::record_cost_usd`] uses.
fn micro_usd_of(model: &str, usage: TokenUsage) -> u64 {
    let usd = crate::llm::pricing_for(model)
        .map(|p| p.cost_usd(usage))
        .unwrap_or(0.0);
    (usd * 1_000_000.0).round() as u64
}

/// Cumulative LLM usage for one live HTTP session, updated lock-free.
///
/// One `AtomicU64` per billing counter keeps the hot path — the end of every
/// turn, and every `/metrics` scrape — free of locks, exactly like the token
/// counters it replaces. USD is accumulated *per turn*, at the model that
/// actually ran it, so a restart onto a different server model prices history
/// at what it cost and new turns at what they cost — token totals alone cannot
/// do both.
#[derive(Debug)]
pub struct SessionUsage {
    model: String,
    prompt_tokens: AtomicU64,
    completion_tokens: AtomicU64,
    cache_hit_tokens: AtomicU64,
    cache_miss_tokens: AtomicU64,
    reasoning_tokens: AtomicU64,
    total_tokens: AtomicU64,
    llm_latency_ms: AtomicU64,
    /// Billed USD in micro-USD (`1e-6 USD`), like `Metrics::cost_micro_usd_total`.
    cost_micro_usd: AtomicU64,
}

impl SessionUsage {
    /// Create a zeroed accumulator priced against `model`.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            prompt_tokens: AtomicU64::new(0),
            completion_tokens: AtomicU64::new(0),
            cache_hit_tokens: AtomicU64::new(0),
            cache_miss_tokens: AtomicU64::new(0),
            reasoning_tokens: AtomicU64::new(0),
            total_tokens: AtomicU64::new(0),
            llm_latency_ms: AtomicU64::new(0),
            cost_micro_usd: AtomicU64::new(0),
        }
    }

    /// Model this session's *new* turns are priced against.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Fold one turn's usage + LLM latency into the running totals.
    pub fn record(&self, usage: TokenUsage, latency_ms: u64) {
        self.prompt_tokens
            .fetch_add(u64::from(usage.prompt_tokens), Ordering::Relaxed);
        self.completion_tokens
            .fetch_add(u64::from(usage.completion_tokens), Ordering::Relaxed);
        self.cache_hit_tokens
            .fetch_add(u64::from(usage.cache_hit_tokens), Ordering::Relaxed);
        self.cache_miss_tokens
            .fetch_add(u64::from(usage.cache_miss_tokens), Ordering::Relaxed);
        self.reasoning_tokens
            .fetch_add(u64::from(usage.reasoning_tokens), Ordering::Relaxed);
        self.total_tokens
            .fetch_add(u64::from(usage.total_tokens), Ordering::Relaxed);
        self.llm_latency_ms.fetch_add(latency_ms, Ordering::Relaxed);
        self.cost_micro_usd
            .fetch_add(micro_usd_of(&self.model, usage), Ordering::Relaxed);
    }

    /// Billed USD so far, summed per turn at the model that ran it.
    ///
    /// `None` while nothing has accrued *and* the current model has no pricing
    /// entry (never a silent `0.0`, which would read as "free"). A session
    /// whose history was billed keeps its real total even if the server later
    /// restarted onto an unpriced model.
    pub fn cost_usd(&self) -> Option<f64> {
        let micro_usd = self.cost_micro_usd.load(Ordering::Relaxed);
        if micro_usd == 0 && crate::llm::pricing_for(&self.model).is_none() {
            return None;
        }
        Some(micro_usd as f64 / 1_000_000.0)
    }

    /// Read the current totals as a plain snapshot.
    pub fn snapshot(&self) -> UsageTotals {
        UsageTotals {
            prompt_tokens: self.prompt_tokens.load(Ordering::Relaxed),
            completion_tokens: self.completion_tokens.load(Ordering::Relaxed),
            cache_hit_tokens: self.cache_hit_tokens.load(Ordering::Relaxed),
            cache_miss_tokens: self.cache_miss_tokens.load(Ordering::Relaxed),
            reasoning_tokens: self.reasoning_tokens.load(Ordering::Relaxed),
            total_tokens: self.total_tokens.load(Ordering::Relaxed),
            llm_latency_ms: self.llm_latency_ms.load(Ordering::Relaxed),
        }
    }

    /// Seed the billed USD from a persisted snapshot (cold load).
    ///
    /// Overwrites rather than adds, like [`Self::restore`]: the persisted
    /// figure *is* the accumulated cost, at the rates history was billed at.
    pub fn restore_cost(&self, micro_usd: u64) {
        self.cost_micro_usd.store(micro_usd, Ordering::Relaxed);
    }

    /// Seed the counters from a persisted snapshot (cold load).
    ///
    /// Overwrites rather than adds: the snapshot *is* the accumulated state.
    pub fn restore(&self, totals: &UsageTotals) {
        self.prompt_tokens
            .store(totals.prompt_tokens, Ordering::Relaxed);
        self.completion_tokens
            .store(totals.completion_tokens, Ordering::Relaxed);
        self.cache_hit_tokens
            .store(totals.cache_hit_tokens, Ordering::Relaxed);
        self.cache_miss_tokens
            .store(totals.cache_miss_tokens, Ordering::Relaxed);
        self.reasoning_tokens
            .store(totals.reasoning_tokens, Ordering::Relaxed);
        self.total_tokens
            .store(totals.total_tokens, Ordering::Relaxed);
        self.llm_latency_ms
            .store(totals.llm_latency_ms, Ordering::Relaxed);
    }
}

/// Serializable snapshot of a [`SessionUsage`].
///
/// Counts are `u64` (a session accumulates across turns, so it can exceed the
/// `u32` a single provider response carries); [`Self::to_token_usage`]
/// saturates back to the provider-level type for pricing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTotals {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_hit_tokens: u64,
    pub cache_miss_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub llm_latency_ms: u64,
}

impl UsageTotals {
    /// True when nothing has been recorded (used to skip persisting empties).
    pub fn is_zero(&self) -> bool {
        *self == Self::default()
    }

    /// Widen a single provider response's usage into the accumulated shape.
    pub fn from_token_usage(usage: &TokenUsage) -> Self {
        Self {
            prompt_tokens: u64::from(usage.prompt_tokens),
            completion_tokens: u64::from(usage.completion_tokens),
            cache_hit_tokens: u64::from(usage.cache_hit_tokens),
            cache_miss_tokens: u64::from(usage.cache_miss_tokens),
            reasoning_tokens: u64::from(usage.reasoning_tokens),
            total_tokens: u64::from(usage.total_tokens),
            llm_latency_ms: 0,
        }
    }

    /// Saturating narrowing to the type the pricing function consumes.
    pub fn to_token_usage(&self) -> TokenUsage {
        fn clamp(v: u64) -> u32 {
            v.min(u64::from(u32::MAX)) as u32
        }
        TokenUsage {
            prompt_tokens: clamp(self.prompt_tokens),
            completion_tokens: clamp(self.completion_tokens),
            total_tokens: clamp(self.total_tokens),
            cache_hit_tokens: clamp(self.cache_hit_tokens),
            cache_miss_tokens: clamp(self.cache_miss_tokens),
            reasoning_tokens: clamp(self.reasoning_tokens),
        }
    }
}

/// USD cost of `totals` at `model`'s rates — the same pricing the CLI and
/// AG-UI channels bill with. `None` when the model has no pricing entry
/// (never a silent `0.0`, which would read as "free").
pub fn cost_usd(model: &str, totals: &UsageTotals) -> Option<f64> {
    crate::llm::pricing_for(model).map(|p| p.cost_usd(totals.to_token_usage()))
}

/// Response body for `GET /sessions/:id/usage` (issue #114).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageResponse {
    pub session_id: String,
    /// Model this session's *new* turns are billed at. Restored history keeps
    /// the cost it was billed at, which a server restart onto another model
    /// does not reprice.
    pub model: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Input tokens served from the provider's prompt cache.
    pub cache_hit_tokens: u64,
    /// Input tokens billed at the full rate (`prompt = hit + miss`).
    pub cache_miss_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    /// Accumulated LLM latency across turns, in milliseconds.
    pub llm_latency_ms: u64,
    /// Billed USD so far, summed per turn; `null` when nothing could be priced.
    pub cost_usd: Option<f64>,
}

impl UsageResponse {
    /// Snapshot a live session's accumulator into the API shape.
    pub fn from_usage(session_id: &str, usage: &SessionUsage) -> Self {
        let totals = usage.snapshot();
        Self {
            session_id: session_id.to_string(),
            model: usage.model().to_string(),
            prompt_tokens: totals.prompt_tokens,
            completion_tokens: totals.completion_tokens,
            cache_hit_tokens: totals.cache_hit_tokens,
            cache_miss_tokens: totals.cache_miss_tokens,
            reasoning_tokens: totals.reasoning_tokens,
            total_tokens: totals.total_tokens,
            llm_latency_ms: totals.llm_latency_ms,
            cost_usd: usage.cost_usd(),
        }
    }
}

/// Storage key for a session's persisted usage snapshot.
pub(super) fn usage_key(id: &str) -> String {
    format!("session-usage/{id}")
}

/// The persisted shape: the token snapshot plus the USD already billed.
///
/// The cost rides along because USD is accumulated per turn at the model that
/// ran it — re-deriving it at restore time from the *current* server model
/// would silently re-bill history at rates it never cost.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct PersistedUsage {
    #[serde(default)]
    pub usage: UsageTotals,
    /// Billed USD in micro-USD, matching [`SessionUsage`]'s accumulator.
    #[serde(default)]
    pub cost_micro_usd: u64,
}

/// Best-effort persist of a session's accumulated usage: a storage failure is
/// logged, never fatal — the session is still usable, only its usage would not
/// survive a restart (the same contract as the #98 metadata blob).
///
/// Zero totals are not written: a session that never ran a turn has nothing
/// worth restoring.
pub(super) async fn persist_usage(
    storage: &Arc<dyn StorageBackend>,
    id: &str,
    usage: &SessionUsage,
) {
    let totals = usage.snapshot();
    if totals.is_zero() {
        return;
    }
    let snapshot = PersistedUsage {
        usage: totals,
        cost_micro_usd: usage.cost_micro_usd.load(Ordering::Relaxed),
    };
    let Ok(json) = serde_json::to_string(&snapshot) else {
        tracing::warn!(session_id = %id, "failed to serialize session usage");
        return;
    };
    if let Err(e) = storage.save_memory(&usage_key(id), &json).await {
        tracing::warn!(session_id = %id, error = %e, "failed to persist session usage");
    }
}

/// Best-effort load of a session's persisted usage. A read or parse failure
/// degrades to `None` (a cold-loaded session starts at zero) — a corrupt blob
/// must not make a restorable session 500.
pub(super) async fn load_persisted_usage(
    storage: &Arc<dyn StorageBackend>,
    id: &str,
) -> Option<PersistedUsage> {
    match storage.load_memory(&usage_key(id)).await {
        Ok(Some(json)) => match serde_json::from_str(&json) {
            Ok(usage) => Some(usage),
            Err(e) => {
                tracing::warn!(
                    session_id = %id,
                    error = %e,
                    "ignoring unparsable session usage"
                );
                None
            }
        },
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(session_id = %id, error = %e, "failed to load session usage");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;
    use crate::storage::LocalStorageBackend;

    fn usage_with(
        prompt: u32,
        completion: u32,
        cache_hit: u32,
        cache_miss: u32,
        reasoning: u32,
    ) -> TokenUsage {
        TokenUsage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            cache_hit_tokens: cache_hit,
            cache_miss_tokens: cache_miss,
            reasoning_tokens: reasoning,
        }
    }

    #[test]
    fn new_session_usage_is_zero_and_keeps_model() {
        let u = SessionUsage::new("deepseek-chat");
        assert_eq!(u.model(), "deepseek-chat");
        assert_eq!(u.snapshot(), UsageTotals::default());
        assert!(u.snapshot().is_zero());
    }

    /// Every counter is distinct, so a swapped field or a dropped accumulator
    /// changes the snapshot.
    #[test]
    fn record_fills_every_counter_independently() {
        let u = SessionUsage::new("m");
        u.record(usage_with(11, 22, 33, 44, 55), 999);
        let t = u.snapshot();
        assert_eq!(t.prompt_tokens, 11);
        assert_eq!(t.completion_tokens, 22);
        assert_eq!(t.cache_hit_tokens, 33);
        assert_eq!(t.cache_miss_tokens, 44);
        assert_eq!(t.reasoning_tokens, 55);
        assert_eq!(t.total_tokens, 33);
        assert_eq!(t.llm_latency_ms, 999);
    }

    #[test]
    fn record_accumulates_across_turns() {
        let u = SessionUsage::new("m");
        u.record(usage_with(10, 5, 1, 9, 0), 100);
        u.record(usage_with(20, 7, 2, 18, 3), 250);
        let t = u.snapshot();
        assert_eq!(t.prompt_tokens, 30);
        assert_eq!(t.completion_tokens, 12);
        assert_eq!(t.cache_hit_tokens, 3);
        assert_eq!(t.cache_miss_tokens, 27);
        assert_eq!(t.reasoning_tokens, 3);
        assert_eq!(t.total_tokens, 42);
        assert_eq!(t.llm_latency_ms, 350);
    }

    /// USD is billed per turn, at the model that ran it — summing per-turn
    /// costs is what lets a restart stop re-pricing history.
    #[test]
    fn record_accumulates_usd_per_turn() {
        let home = tempfile::tempdir().unwrap();
        let _pin = crate::test_util::PinnedRecursiveHome::new(home.path());
        let u = SessionUsage::new("deepseek-chat");
        // 1M completion tokens at $0.28/M.
        u.record(usage_with(0, 1_000_000, 0, 0, 0), 0);
        assert_eq!(u.cost_usd(), Some(0.28));
        u.record(usage_with(0, 1_000_000, 0, 0, 0), 0);
        assert_eq!(u.cost_usd(), Some(0.56));
    }

    /// A restore replaces the frozen USD rather than adding to it.
    #[test]
    fn restore_cost_overwrites() {
        let home = tempfile::tempdir().unwrap();
        let _pin = crate::test_util::PinnedRecursiveHome::new(home.path());
        let u = SessionUsage::new("deepseek-chat");
        u.record(usage_with(0, 1_000_000, 0, 0, 0), 0);
        u.restore_cost(5_000_000);
        assert_eq!(u.cost_usd(), Some(5.0));
    }

    #[test]
    fn restore_replaces_the_counters() {
        let u = SessionUsage::new("m");
        u.record(usage_with(1, 1, 1, 1, 1), 1);
        let seeded = UsageTotals {
            prompt_tokens: 100,
            completion_tokens: 200,
            cache_hit_tokens: 300,
            cache_miss_tokens: 400,
            reasoning_tokens: 500,
            total_tokens: 600,
            llm_latency_ms: 700,
        };
        u.restore(&seeded);
        assert_eq!(u.snapshot(), seeded, "restore must overwrite, not add");
    }

    #[test]
    fn from_token_usage_widens_every_field() {
        let t = UsageTotals::from_token_usage(&usage_with(7, 8, 9, 10, 11));
        assert_eq!(t.prompt_tokens, 7);
        assert_eq!(t.completion_tokens, 8);
        assert_eq!(t.cache_hit_tokens, 9);
        assert_eq!(t.cache_miss_tokens, 10);
        assert_eq!(t.reasoning_tokens, 11);
        assert_eq!(t.total_tokens, 15);
        assert_eq!(t.llm_latency_ms, 0);
    }

    #[test]
    fn cost_usd_prices_a_known_model() {
        // Pin RECURSIVE_HOME so the effective catalog collapses to the bundled
        // prices (a stray providers cache on the dev machine must not change them).
        let home = tempfile::tempdir().unwrap();
        let _pin = crate::test_util::PinnedRecursiveHome::new(home.path());
        // deepseek-chat: $0.14/M input, $0.28/M output, $0.0028/M cache hit.
        let totals = UsageTotals {
            prompt_tokens: 1_000_000,
            completion_tokens: 500_000,
            cache_hit_tokens: 0,
            cache_miss_tokens: 1_000_000,
            total_tokens: 1_500_000,
            reasoning_tokens: 0,
            llm_latency_ms: 0,
        };
        let cost = cost_usd("deepseek-chat", &totals).expect("deepseek-chat is priced");
        assert!((cost - 0.28).abs() < 1e-9, "got {cost}");
    }

    #[test]
    fn cost_usd_applies_the_cache_discount() {
        let home = tempfile::tempdir().unwrap();
        let _pin = crate::test_util::PinnedRecursiveHome::new(home.path());
        // deepseek-chat: hit 600k * 0.0028/M, miss 400k * 0.14/M, out 500k * 0.28/M.
        let totals = UsageTotals {
            prompt_tokens: 1_000_000,
            completion_tokens: 500_000,
            cache_hit_tokens: 600_000,
            cache_miss_tokens: 400_000,
            total_tokens: 1_500_000,
            reasoning_tokens: 0,
            llm_latency_ms: 0,
        };
        let cost = cost_usd("deepseek-chat", &totals).expect("priced");
        assert!((cost - 0.197_68).abs() < 1e-9, "got {cost}");
    }

    /// The HTTP channel must bill exactly what `CostTracker` bills — that is
    /// the "same-source accounting" the issue asks for.
    #[test]
    fn cost_usd_matches_cost_tracker() {
        let _home = tempfile::tempdir().unwrap();
        let _pin = crate::test_util::PinnedRecursiveHome::new(_home.path());
        let dir = tempfile::tempdir().unwrap();
        let raw = usage_with(1_234, 567, 89, 1_145, 12);
        let mut tracker =
            crate::cost::CostTracker::new(dir.path().to_path_buf(), "deepseek-chat", "openai");
        tracker.record_usage(raw, 0);

        let u = SessionUsage::new("deepseek-chat");
        u.record(raw, 0);
        let ours = cost_usd("deepseek-chat", &u.snapshot()).expect("priced");
        assert!(
            (ours - tracker.cost_usd().expect("tracker priced")).abs() < 1e-12,
            "HTTP and CostTracker must agree: {ours}"
        );
    }

    #[test]
    fn cost_usd_is_none_for_unpriced_model() {
        let totals = UsageTotals {
            prompt_tokens: 100,
            total_tokens: 100,
            ..Default::default()
        };
        assert!(cost_usd("no-such-model-v42", &totals).is_none());
    }

    #[test]
    fn to_token_usage_saturates_at_u32_max() {
        let totals = UsageTotals {
            prompt_tokens: u64::from(u32::MAX) + 10,
            completion_tokens: 5,
            cache_hit_tokens: 1,
            cache_miss_tokens: 2,
            reasoning_tokens: 3,
            total_tokens: 4,
            llm_latency_ms: 0,
        };
        let tu = totals.to_token_usage();
        assert_eq!(tu.prompt_tokens, u32::MAX);
        assert_eq!(tu.completion_tokens, 5);
        assert_eq!(tu.cache_hit_tokens, 1);
        assert_eq!(tu.cache_miss_tokens, 2);
        assert_eq!(tu.reasoning_tokens, 3);
        assert_eq!(tu.total_tokens, 4);
    }

    #[test]
    fn usage_key_is_namespaced_per_session() {
        assert_eq!(usage_key("abc"), "session-usage/abc");
    }

    #[tokio::test]
    async fn persist_and_load_roundtrip() {
        let home = tempfile::tempdir().unwrap();
        let _pin = crate::test_util::PinnedRecursiveHome::new(home.path());
        let dir = tempfile::tempdir().unwrap();
        let storage: Arc<dyn StorageBackend> =
            Arc::new(LocalStorageBackend::new(dir.path().to_path_buf()));
        let u = SessionUsage::new("deepseek-chat");
        u.record(usage_with(10, 20, 3, 7, 1), 42);
        assert!(u.cost_usd().expect("priced") > 0.0);

        persist_usage(&storage, "sess-1", &u).await;

        let loaded = load_persisted_usage(&storage, "sess-1")
            .await
            .expect("persisted usage");
        assert_eq!(loaded.usage, u.snapshot());
        assert_eq!(
            loaded.cost_micro_usd,
            u.cost_micro_usd.load(Ordering::Relaxed),
            "the billed USD must ride the snapshot, not be re-derived later"
        );
    }

    #[tokio::test]
    async fn zero_usage_is_not_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let storage: Arc<dyn StorageBackend> =
            Arc::new(LocalStorageBackend::new(dir.path().to_path_buf()));
        let u = SessionUsage::new("m");
        persist_usage(&storage, "sess-zero", &u).await;
        assert!(
            load_persisted_usage(&storage, "sess-zero").await.is_none(),
            "a session with no turns must not leave an empty blob behind"
        );
    }

    #[tokio::test]
    async fn load_missing_usage_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let storage: Arc<dyn StorageBackend> =
            Arc::new(LocalStorageBackend::new(dir.path().to_path_buf()));
        assert!(load_persisted_usage(&storage, "absent").await.is_none());
    }

    /// A storage read failure degrades to "no persisted usage" rather than
    /// propagating — a broken backend must not 500 a restorable session.
    #[tokio::test]
    async fn load_persisted_usage_degrades_on_storage_error() {
        struct ReadFailing;
        #[async_trait::async_trait]
        impl StorageBackend for ReadFailing {
            async fn load_transcript(&self, _id: &str) -> crate::error::Result<Vec<Message>> {
                Ok(vec![])
            }
            async fn save_transcript(
                &self,
                _id: &str,
                _msgs: &[Message],
            ) -> crate::error::Result<()> {
                Ok(())
            }
            async fn load_memory(&self, _k: &str) -> crate::error::Result<Option<String>> {
                Err(crate::error::Error::Storage {
                    message: "unavailable".into(),
                })
            }
            async fn save_memory(&self, _k: &str, _v: &str) -> crate::error::Result<()> {
                Ok(())
            }
        }
        let storage: Arc<dyn StorageBackend> = Arc::new(ReadFailing);
        assert!(load_persisted_usage(&storage, "x").await.is_none());
    }

    #[tokio::test]
    async fn corrupt_usage_blob_degrades_to_none() {
        let dir = tempfile::tempdir().unwrap();
        let storage: Arc<dyn StorageBackend> =
            Arc::new(LocalStorageBackend::new(dir.path().to_path_buf()));
        storage
            .save_memory(&usage_key("bad"), "{not json")
            .await
            .unwrap();
        assert!(load_persisted_usage(&storage, "bad").await.is_none());
    }

    /// A read-only backend must not make persistence fatal — the turn already
    /// happened, the usage just will not survive a restart.
    #[tokio::test]
    async fn persist_usage_swallows_storage_errors() {
        struct Failing;
        #[async_trait::async_trait]
        impl StorageBackend for Failing {
            async fn load_transcript(&self, _id: &str) -> crate::error::Result<Vec<Message>> {
                Ok(vec![])
            }
            async fn save_transcript(
                &self,
                _id: &str,
                _msgs: &[Message],
            ) -> crate::error::Result<()> {
                Ok(())
            }
            async fn load_memory(&self, _k: &str) -> crate::error::Result<Option<String>> {
                Ok(None)
            }
            async fn save_memory(&self, _k: &str, _v: &str) -> crate::error::Result<()> {
                Err(crate::error::Error::Storage {
                    message: "read-only".into(),
                })
            }
        }
        let storage: Arc<dyn StorageBackend> = Arc::new(Failing);
        let u = SessionUsage::new("m");
        u.record(usage_with(1, 1, 0, 1, 0), 1);
        // Must not panic.
        persist_usage(&storage, "s", &u).await;
    }

    #[test]
    fn usage_response_carries_the_cache_split_and_cost() {
        let home = tempfile::tempdir().unwrap();
        let _pin = crate::test_util::PinnedRecursiveHome::new(home.path());
        let u = SessionUsage::new("deepseek-chat");
        u.record(usage_with(1_000_000, 500_000, 600_000, 400_000, 0), 123);
        let resp = UsageResponse::from_usage("sess-9", &u);
        assert_eq!(resp.session_id, "sess-9");
        assert_eq!(resp.model, "deepseek-chat");
        assert_eq!(resp.prompt_tokens, 1_000_000);
        assert_eq!(resp.completion_tokens, 500_000);
        assert_eq!(resp.cache_hit_tokens, 600_000);
        assert_eq!(resp.cache_miss_tokens, 400_000);
        assert_eq!(resp.total_tokens, 1_500_000);
        assert_eq!(resp.llm_latency_ms, 123);
        assert!((resp.cost_usd.expect("priced") - 0.197_68).abs() < 1e-9);
    }

    #[test]
    fn usage_response_cost_is_null_for_unpriced_model() {
        let u = SessionUsage::new("no-such-model-v42");
        u.record(usage_with(1, 1, 0, 1, 0), 0);
        let resp = UsageResponse::from_usage("s", &u);
        assert!(resp.cost_usd.is_none());
        let json = serde_json::to_value(&resp).unwrap();
        assert!(
            json["cost_usd"].is_null(),
            "unpriced cost must serialise as null"
        );
        assert_eq!(json["cache_hit_tokens"], 0);
    }
}
