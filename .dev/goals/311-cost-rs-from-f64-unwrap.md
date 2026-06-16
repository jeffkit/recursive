# Goal 311 — Fix serde_json Number::from_f64(0.0).unwrap() in cost.rs

**Roadmap**: Post-Phase (Invariant #5 compliance)

**Design principle check**:
- Implemented as: replacing `.unwrap()` on `serde_json::Number::from_f64(0.0)`
  with `serde_json::Number::from(0u64)` in `src/cost.rs`
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

In `src/cost.rs`, the `update_meta_with_cost()` method (line ~169) contains:

```rust
serde_json::Number::from_f64(self.cost_usd().unwrap_or(0.0))
    .unwrap_or(serde_json::Number::from_f64(0.0).unwrap()),
```

The innermost `.unwrap()` calls `serde_json::Number::from_f64(0.0)`.
`from_f64()` returns `Option<Number>` — it returns `None` for NaN and
Infinity, but `Some` for all other values. While `0.0` is a valid finite
float and this will never panic in practice, it is still an **Invariant #5
violation** (no `unwrap()` in non-test product code).

The simple fix: replace `serde_json::Number::from_f64(0.0).unwrap()` with
`serde_json::Number::from(0u64)`, which constructs the integer `0` as a
JSON Number without any potential for failure.

## Scope (do exactly this, no more)

### 1. `src/cost.rs` — fix line ~169

Change:
```rust
serde_json::Number::from_f64(self.cost_usd().unwrap_or(0.0))
    .unwrap_or(serde_json::Number::from_f64(0.0).unwrap()),
```

To:
```rust
serde_json::Number::from_f64(self.cost_usd().unwrap_or(0.0))
    .unwrap_or(serde_json::Number::from(0u64)),
```

This is a one-word change (`from_f64(0.0).unwrap()` → `from(0u64)`).

### 2. Tests

Add a unit test in `src/cost.rs` that calls `update_meta_with_cost()` and
verifies it succeeds (does not panic) even when `cost_usd()` would return
NaN or when the JSON Number construction could fail:

```rust
#[test]
fn update_meta_with_cost_no_panic() {
    let dir = tempfile::tempdir().unwrap();
    // Write a valid meta.json
    std::fs::write(dir.path().join(".meta.json"), r#"{"some": "field"}"#).unwrap();
    let mut tracker = CostTracker::new(dir.path(), "test-model", "openai");
    // Don't crash even with no usage recorded
    let res = tracker.update_meta_with_cost();
    assert!(res.is_ok());
}
```

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `grep -n "from_f64.*0\.0.*unwrap\b" src/cost.rs` returns no results

## Notes for the agent

- Read `src/cost.rs` around lines 145–180 for full context of
  `update_meta_with_cost()`.
- The fix is ONE character change: replace `from_f64(0.0).unwrap()` with
  `from(0u64)`. Both represent the JSON number `0`.
- `serde_json::Number::from(0u64)` always succeeds (it's an integer).
- **DO NOT modify** `src/agent.rs`, `src/runtime.rs`, `src/kernel.rs`,
  `src/http/`, or any other files.
