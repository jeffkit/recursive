//! Regression tests for issue #42: unknown pricing must not be rendered as 0.0.
//!
//! 1. `cost.json` `cost_usd` must be `null` (not 0.0) when the model has no
//!    pricing entry.
//! 2. `.meta.json` `cost_usd` must be `null` (not 0.0) for unknown pricing.
//! 3. Priced models keep numeric output in both files.

use recursive::cost::CostTracker;
use recursive::llm::TokenUsage;

fn usage() -> TokenUsage {
    TokenUsage {
        prompt_tokens: 1000,
        completion_tokens: 500,
        total_tokens: 1500,
        ..Default::default()
    }
}

#[test]
fn cost_json_unknown_model_cost_usd_is_null() {
    let home = tempfile::tempdir().unwrap();
    let _pin = recursive::test_util::PinnedRecursiveHome::new(home.path());
    let dir = tempfile::tempdir().unwrap();
    let mut tracker = CostTracker::new(dir.path().to_path_buf(), "no-such-model-v42", "openai");
    tracker.record_usage(usage(), 100);
    tracker.finish().unwrap();

    let raw = std::fs::read_to_string(dir.path().join("cost.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert!(
        v["cost_usd"].is_null(),
        "cost.json cost_usd must be null for unpriced model, got: {}",
        v["cost_usd"]
    );
    assert!(v["pricing"].is_null());
}

#[test]
fn meta_json_unknown_model_cost_usd_is_null() {
    let home = tempfile::tempdir().unwrap();
    let _pin = recursive::test_util::PinnedRecursiveHome::new(home.path());
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".meta.json"), r#"{"session_id": "s1"}"#).unwrap();
    let mut tracker = CostTracker::new(dir.path().to_path_buf(), "no-such-model-v42", "openai");
    tracker.record_usage(usage(), 100);
    tracker.finish().unwrap();

    let raw = std::fs::read_to_string(dir.path().join(".meta.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert!(
        v["cost_usd"].is_null(),
        ".meta.json cost_usd must be null for unpriced model, got: {}",
        v["cost_usd"]
    );
}

#[test]
fn priced_model_cost_json_and_meta_stay_numeric() {
    let home = tempfile::tempdir().unwrap();
    let _pin = recursive::test_util::PinnedRecursiveHome::new(home.path());
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".meta.json"), r#"{"session_id": "s1"}"#).unwrap();
    let mut tracker = CostTracker::new(dir.path().to_path_buf(), "deepseek-chat", "openai");
    tracker.record_usage(usage(), 100);
    tracker.finish().unwrap();

    let cost: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("cost.json")).unwrap())
            .unwrap();
    assert!(
        cost["cost_usd"].is_number(),
        "priced model must stay numeric"
    );
    assert!(cost["pricing"].is_object());

    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join(".meta.json")).unwrap())
            .unwrap();
    assert!(meta["cost_usd"].is_number());
    // deepseek-chat: $0.14/M in (1000), $0.28/M out (500) → 0.00014 + 0.00014
    let c = meta["cost_usd"].as_f64().unwrap();
    assert!((c - 0.000_28).abs() < 0.000_01, "unexpected cost {c}");
}
