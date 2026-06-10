//! Cost tracking for agent runs.
//!
//! `CostTracker` observes token usage flowing through the agent and accumulates
//! cost data. It writes a `cost.json` file into the session
//! directory alongside the JSONL transcript, and can update the session meta
//! file with cost summary fields.
//!
//! # Usage
//!
//! ```ignore
//! let tracker = CostTracker::new(&workspace, "gpt-4o", "openai");
//! // after runtime.run(...):
//! tracker.record_usage(outcome.total_usage, outcome.llm_latency_ms);
//! tracker.finish()?;
//! ```

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::llm::{pricing_for, ModelPricing, TokenUsage};

/// Accumulated cost data for a single agent run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostData {
    /// Model name used for the run.
    pub model: String,
    /// Provider identifier.
    pub provider: String,
    /// Total token usage across all LLM calls.
    pub total_usage: TokenUsage,
    /// Total LLM latency in milliseconds.
    pub total_llm_latency_ms: u64,
    /// Computed cost in USD (None if pricing is unknown for the model).
    pub cost_usd: Option<f64>,
    /// Pricing rates used for the computation (None if unknown).
    pub pricing: Option<CostPricing>,
}

/// Serializable pricing rates, mirroring `ModelPricing` but serializable.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CostPricing {
    pub input_per_million: f64,
    pub output_per_million: f64,
    pub cache_hit_input_per_million: f64,
}

impl From<ModelPricing> for CostPricing {
    fn from(p: ModelPricing) -> Self {
        Self {
            input_per_million: p.input_per_million,
            output_per_million: p.output_per_million,
            cache_hit_input_per_million: p.cache_hit_input_per_million,
        }
    }
}

/// Tracks token usage and cost across an agent run, persisting to the session
/// directory.
///
/// `CostTracker` is designed to be used alongside `SessionWriter`. It writes a
/// `cost.json` file into the same session directory and optionally updates the
/// `.meta.json` file with cost summary fields.
pub struct CostTracker {
    session_dir: PathBuf,
    model: String,
    provider: String,
    pricing: Option<ModelPricing>,
    /// Accumulated token usage across all LLM calls observed so far.
    accumulated_usage: TokenUsage,
    /// Accumulated LLM latency in milliseconds.
    accumulated_latency_ms: u64,
    /// Whether the tracker has been finished (written final cost.json).
    finished: bool,
}

impl CostTracker {
    /// Create a new `CostTracker` for the given session directory.
    ///
    /// `session_dir` should be the same directory used by `SessionWriter`.
    /// Pricing is looked up from the bundled `providers.toml` via `pricing_for()`.
    pub fn new(session_dir: PathBuf, model: &str, provider: &str) -> Self {
        let pricing = pricing_for(model);
        Self {
            session_dir,
            model: model.to_string(),
            provider: provider.to_string(),
            pricing,
            accumulated_usage: TokenUsage::default(),
            accumulated_latency_ms: 0,
            finished: false,
        }
    }

    /// Record token usage and latency from a single LLM call.
    ///
    /// Call this from the integration layer after each `runtime.run()`,
    /// passing `outcome.total_usage` and `outcome.llm_latency_ms` from
    /// the [`RuntimeOutcome`](crate::runtime::RuntimeOutcome).
    pub fn record_usage(&mut self, usage: TokenUsage, latency_ms: u64) {
        self.accumulated_usage = self.accumulated_usage.accumulate(usage);
        self.accumulated_latency_ms = self.accumulated_latency_ms.saturating_add(latency_ms);
    }

    /// Return the accumulated token usage so far.
    pub fn accumulated_usage(&self) -> TokenUsage {
        self.accumulated_usage
    }

    /// Return the accumulated LLM latency in milliseconds.
    pub fn accumulated_latency_ms(&self) -> u64 {
        self.accumulated_latency_ms
    }

    /// Compute the cost in USD for the accumulated usage.
    ///
    /// Returns `None` if pricing is unknown for the model.
    pub fn cost_usd(&self) -> Option<f64> {
        self.pricing.map(|p| p.cost_usd(self.accumulated_usage))
    }

    /// Build the current `CostData` snapshot.
    pub fn cost_data(&self) -> CostData {
        CostData {
            model: self.model.clone(),
            provider: self.provider.clone(),
            total_usage: self.accumulated_usage,
            total_llm_latency_ms: self.accumulated_latency_ms,
            cost_usd: self.cost_usd(),
            pricing: self.pricing.map(CostPricing::from),
        }
    }

    /// Write the cost data to `cost.json` in the session directory.
    ///
    /// Returns the path to the written file.
    pub fn write_cost_json(&self) -> std::io::Result<PathBuf> {
        let cost_path = self.session_dir.join("cost.json");
        let data = self.cost_data();
        let json = serde_json::to_string_pretty(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        crate::atomic::atomic_write(&cost_path, json.as_bytes())?;
        Ok(cost_path)
    }

    /// Update the `.meta.json` file in the session directory with cost summary
    /// fields.
    ///
    /// Reads the existing meta file, adds cost fields, and writes it back.
    /// If the meta file doesn't exist or can't be read, this is a no-op.
    pub fn update_meta_with_cost(&self) -> std::io::Result<()> {
        let meta_path = self.session_dir.join(".meta.json");
        let existing = match std::fs::read_to_string(&meta_path) {
            Ok(s) => s,
            Err(_) => return Ok(()), // meta file doesn't exist yet, skip
        };

        // Parse existing meta as a generic Value to preserve unknown fields
        let mut meta: serde_json::Value = match serde_json::from_str(&existing) {
            Ok(v) => v,
            Err(_) => return Ok(()),
        };

        if let Some(obj) = meta.as_object_mut() {
            obj.insert(
                "cost_usd".to_string(),
                serde_json::Value::Number(
                    serde_json::Number::from_f64(self.cost_usd().unwrap_or(0.0))
                        .unwrap_or(serde_json::Number::from_f64(0.0).unwrap()),
                ),
            );
            obj.insert(
                "total_tokens".to_string(),
                serde_json::Value::Number(serde_json::Number::from(
                    self.accumulated_usage.total_tokens,
                )),
            );
            obj.insert(
                "prompt_tokens".to_string(),
                serde_json::Value::Number(serde_json::Number::from(
                    self.accumulated_usage.prompt_tokens,
                )),
            );
            obj.insert(
                "completion_tokens".to_string(),
                serde_json::Value::Number(serde_json::Number::from(
                    self.accumulated_usage.completion_tokens,
                )),
            );
            obj.insert(
                "cache_hit_tokens".to_string(),
                serde_json::Value::Number(serde_json::Number::from(
                    self.accumulated_usage.cache_hit_tokens,
                )),
            );
            obj.insert(
                "cache_miss_tokens".to_string(),
                serde_json::Value::Number(serde_json::Number::from(
                    self.accumulated_usage.cache_miss_tokens,
                )),
            );
            obj.insert(
                "total_llm_latency_ms".to_string(),
                serde_json::Value::Number(serde_json::Number::from(self.accumulated_latency_ms)),
            );
        }

        let updated = serde_json::to_string_pretty(&meta)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        crate::atomic::atomic_write(&meta_path, updated.as_bytes())
    }

    /// Finalise the tracker: write `cost.json` and update `.meta.json`.
    ///
    /// After calling this, the tracker is marked as finished and subsequent
    /// calls are no-ops.
    pub fn finish(&mut self) -> std::io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;

        self.write_cost_json()?;
        self.update_meta_with_cost()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::TokenUsage;

    #[test]
    fn test_cost_tracker_new() {
        let dir = tempfile::tempdir().unwrap();
        let tracker = CostTracker::new(dir.path().to_path_buf(), "deepseek-chat", "openai");
        assert_eq!(tracker.model, "deepseek-chat");
        assert_eq!(tracker.provider, "openai");
        assert!(tracker.pricing.is_some());
        assert_eq!(tracker.accumulated_usage.total_tokens, 0);
        assert!(!tracker.finished);
    }

    #[test]
    fn test_cost_tracker_record_usage() {
        let dir = tempfile::tempdir().unwrap();
        let mut tracker = CostTracker::new(dir.path().to_path_buf(), "deepseek-chat", "openai");

        let usage1 = TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            cache_hit_tokens: 0,
            cache_miss_tokens: 100,
        };
        tracker.record_usage(usage1, 500);

        assert_eq!(tracker.accumulated_usage.prompt_tokens, 100);
        assert_eq!(tracker.accumulated_usage.completion_tokens, 50);
        assert_eq!(tracker.accumulated_usage.total_tokens, 150);
        assert_eq!(tracker.accumulated_latency_ms, 500);

        let usage2 = TokenUsage {
            prompt_tokens: 200,
            completion_tokens: 100,
            total_tokens: 300,
            cache_hit_tokens: 0,
            cache_miss_tokens: 200,
        };
        tracker.record_usage(usage2, 300);

        assert_eq!(tracker.accumulated_usage.prompt_tokens, 300);
        assert_eq!(tracker.accumulated_usage.completion_tokens, 150);
        assert_eq!(tracker.accumulated_usage.total_tokens, 450);
        assert_eq!(tracker.accumulated_latency_ms, 800);
    }

    #[test]
    fn test_cost_tracker_cost_usd() {
        let dir = tempfile::tempdir().unwrap();
        let mut tracker = CostTracker::new(dir.path().to_path_buf(), "deepseek-chat", "openai");

        // deepseek-chat pricing: $0.27/M input, $1.10/M output
        let usage = TokenUsage {
            prompt_tokens: 1_000_000,
            completion_tokens: 500_000,
            total_tokens: 1_500_000,
            cache_hit_tokens: 0,
            cache_miss_tokens: 1_000_000,
        };
        tracker.record_usage(usage, 0);

        let cost = tracker.cost_usd().unwrap();
        // input: 1M * 0.14/1M = $0.14
        // output: 500k * 0.28/1M = $0.14
        // total: $0.28
        assert!((cost - 0.28).abs() < 0.001);
    }

    #[test]
    fn test_cost_tracker_unknown_model() {
        let dir = tempfile::tempdir().unwrap();
        let tracker = CostTracker::new(dir.path().to_path_buf(), "nonexistent-model-v42", "openai");
        assert!(tracker.pricing.is_none());
        assert!(tracker.cost_usd().is_none());
    }

    #[test]
    fn test_cost_tracker_write_cost_json() {
        let dir = tempfile::tempdir().unwrap();
        let mut tracker = CostTracker::new(dir.path().to_path_buf(), "deepseek-chat", "openai");

        let usage = TokenUsage {
            prompt_tokens: 1000,
            completion_tokens: 500,
            total_tokens: 1500,
            cache_hit_tokens: 200,
            cache_miss_tokens: 800,
        };
        tracker.record_usage(usage, 1234);

        let path = tracker.write_cost_json().unwrap();
        assert!(path.exists());
        assert_eq!(path.file_name().unwrap(), "cost.json");

        // Read back and verify
        let contents = std::fs::read_to_string(&path).unwrap();
        let data: CostData = serde_json::from_str(&contents).unwrap();
        assert_eq!(data.model, "deepseek-chat");
        assert_eq!(data.total_usage.prompt_tokens, 1000);
        assert_eq!(data.total_usage.completion_tokens, 500);
        assert_eq!(data.total_usage.total_tokens, 1500);
        assert_eq!(data.total_usage.cache_hit_tokens, 200);
        assert_eq!(data.total_usage.cache_miss_tokens, 800);
        assert_eq!(data.total_llm_latency_ms, 1234);
        assert!(data.cost_usd.is_some());
        assert!(data.pricing.is_some());
    }

    #[test]
    fn test_cost_tracker_finish_writes_files() {
        let dir = tempfile::tempdir().unwrap();

        // Create a minimal .meta.json to test update_meta_with_cost
        let initial_meta = serde_json::json!({
            "session_id": "test-session",
            "goal": "test goal",
            "model": "deepseek-chat",
            "provider": "openai",
            "created_at": "2025-01-01T00:00:00Z",
            "updated_at": "2025-01-01T00:00:00Z",
            "message_count": 10,
            "status": "active",
        });
        let meta_path = dir.path().join(".meta.json");
        crate::atomic::atomic_write(
            &meta_path,
            serde_json::to_string_pretty(&initial_meta)
                .unwrap()
                .as_bytes(),
        )
        .unwrap();

        let mut tracker = CostTracker::new(dir.path().to_path_buf(), "deepseek-chat", "openai");

        let usage = TokenUsage {
            prompt_tokens: 500,
            completion_tokens: 300,
            total_tokens: 800,
            cache_hit_tokens: 100,
            cache_miss_tokens: 400,
        };
        tracker.record_usage(usage, 999);

        tracker.finish().unwrap();

        // Verify cost.json exists
        let cost_path = dir.path().join("cost.json");
        assert!(cost_path.exists());

        // Verify .meta.json was updated with cost fields
        let updated_meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
        // deepseek-chat: input $0.14/M, output $0.28/M, cache hit $0.0028/M
        // cache hit: 100 * 0.0028/1M = 0.00000028
        // cache miss: 400 * 0.14/1M = 0.000056
        // output: 300 * 0.28/1M = 0.000084
        // total: 0.000140308
        assert!((updated_meta["cost_usd"].as_f64().unwrap() - 0.000_140_308).abs() < 0.000_001);
        assert_eq!(updated_meta["total_tokens"], 800);
        assert_eq!(updated_meta["prompt_tokens"], 500);
        assert_eq!(updated_meta["completion_tokens"], 300);
        assert_eq!(updated_meta["cache_hit_tokens"], 100);
        assert_eq!(updated_meta["cache_miss_tokens"], 400);
        assert_eq!(updated_meta["total_llm_latency_ms"], 999);

        // Verify original fields are preserved
        assert_eq!(updated_meta["session_id"], "test-session");
        assert_eq!(updated_meta["goal"], "test goal");
        assert_eq!(updated_meta["message_count"], 10);
    }

    #[test]
    fn test_cost_tracker_finish_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let mut tracker = CostTracker::new(dir.path().to_path_buf(), "deepseek-chat", "openai");

        let usage = TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            cache_hit_tokens: 0,
            cache_miss_tokens: 100,
        };
        tracker.record_usage(usage, 100);

        // Call finish twice — second call should be a no-op
        tracker.finish().unwrap();
        tracker.finish().unwrap();

        // Only one cost.json should exist
        let cost_path = dir.path().join("cost.json");
        assert!(cost_path.exists());
    }

    #[test]
    fn test_cost_tracker_no_meta_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut tracker = CostTracker::new(dir.path().to_path_buf(), "deepseek-chat", "openai");

        let usage = TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            cache_hit_tokens: 0,
            cache_miss_tokens: 100,
        };
        tracker.record_usage(usage, 100);

        // Should not error even without .meta.json
        tracker.finish().unwrap();
        assert!(dir.path().join("cost.json").exists());
    }

    #[test]
    fn test_cost_data_serialization_roundtrip() {
        let data = CostData {
            model: "test-model".to_string(),
            provider: "test-provider".to_string(),
            total_usage: TokenUsage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                cache_hit_tokens: 10,
                cache_miss_tokens: 90,
            },
            total_llm_latency_ms: 5000,
            cost_usd: Some(0.0123),
            pricing: Some(CostPricing {
                input_per_million: 2.5,
                output_per_million: 10.0,
                cache_hit_input_per_million: 0.25,
            }),
        };

        let json = serde_json::to_string_pretty(&data).unwrap();
        let deserialized: CostData = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.model, "test-model");
        assert_eq!(deserialized.total_usage.prompt_tokens, 100);
        assert_eq!(deserialized.total_usage.completion_tokens, 50);
        assert_eq!(deserialized.total_usage.total_tokens, 150);
        assert_eq!(deserialized.total_usage.cache_hit_tokens, 10);
        assert_eq!(deserialized.total_usage.cache_miss_tokens, 90);
        assert_eq!(deserialized.total_llm_latency_ms, 5000);
        assert!((deserialized.cost_usd.unwrap() - 0.0123).abs() < 0.0001);
        let p = deserialized.pricing.unwrap();
        assert!((p.input_per_million - 2.5).abs() < 0.001);
        assert!((p.output_per_million - 10.0).abs() < 0.001);
        assert!((p.cache_hit_input_per_million - 0.25).abs() < 0.001);
    }

    #[test]
    fn test_cost_tracker_cache_hit_discount() {
        let dir = tempfile::tempdir().unwrap();
        let mut tracker = CostTracker::new(dir.path().to_path_buf(), "deepseek-chat", "openai");

        // deepseek-chat: $0.14/M input, $0.28/M output, $0.0028/M cache hit
        let usage = TokenUsage {
            prompt_tokens: 1_000_000,
            completion_tokens: 500_000,
            total_tokens: 1_500_000,
            cache_hit_tokens: 600_000,
            cache_miss_tokens: 400_000,
        };
        tracker.record_usage(usage, 0);

        let cost = tracker.cost_usd().unwrap();
        // cache hit: 600k * 0.0028/1M = $0.00168
        // cache miss: 400k * 0.14/1M = $0.056
        // output: 500k * 0.28/1M = $0.14
        // total: $0.19768
        assert!((cost - 0.19768).abs() < 0.001);
    }
}
