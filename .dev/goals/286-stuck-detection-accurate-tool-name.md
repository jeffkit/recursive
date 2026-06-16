# Goal 286 — Stuck Detection: Report Most-Frequently-Erroring Tool Name

**Roadmap**: Post-Phase (Correctness) — Bug fix 2/3 from arch-review 2026-06-16

**Design principle check**:
- Implemented as: data structure change inside `run_core.rs` stuck-detection window
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

`RunCore::run_inner` tracks a sliding window of tool-call errors to detect stuck agents.
When the error rate threshold is exceeded, it emits `FinishReason::Stuck { repeated_call, repeats }`.

Currently `repeated_call` is set to `name.clone()` where `name` is the tool from the
**current iteration** of the results loop — the tool that happened to push the window over
threshold. This is often not the most-repeated erroring tool:

- Agent calls tool A (fails), tool B (fails), tool A (fails) → stuck triggers on B, but A errored twice
- Agent calls A, B, A, B in round-robin → stuck reports B because it was the last one checked

The fix tracks tool names alongside error flags, so `repeated_call` can report the tool
that actually appears most frequently in the error window.

## Scope (do exactly this, no more)

### 1. `src/run_core.rs`

Change the type of `recent_errors` from `VecDeque<bool>` to `VecDeque<(bool, String)>`.
The `String` is the tool name for that entry, used only when `bool == true`.

Update all sites that push into the deque:
```rust
// Before:
recent_errors.push_back(is_error);

// After:
recent_errors.push_back((is_error, name.clone()));
```

Update the stuck-detection rate check to compute the most-frequent erroring tool name:
```rust
// Before:
let error_count = recent_errors.iter().filter(|&&e| e).count();
let rate = error_count as f64 / self.stuck_window as f64;
if rate >= self.stuck_error_rate {
    let finish = FinishReason::Stuck {
        repeated_call: name.clone(),
        repeats: error_count,
    };

// After:
let error_entries: Vec<&str> = recent_errors
    .iter()
    .filter(|(is_err, _)| *is_err)
    .map(|(_, n)| n.as_str())
    .collect();
let error_count = error_entries.len();
let rate = error_count as f64 / self.stuck_window as f64;
if rate >= self.stuck_error_rate {
    // Find the most frequently appearing tool name in the error window.
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for n in &error_entries { *counts.entry(n).or_default() += 1; }
    let top_tool = counts
        .into_iter()
        .max_by_key(|&(_, c)| c)
        .map(|(n, _)| n)
        .unwrap_or(name.as_str());
    let finish = FinishReason::Stuck {
        repeated_call: top_tool.to_string(),
        repeats: error_count,
    };
```

Also update the `VecDeque::with_capacity` initialization:
```rust
// Before:
let mut recent_errors: std::collections::VecDeque<bool> =
    std::collections::VecDeque::with_capacity(self.stuck_window);

// After:
let mut recent_errors: std::collections::VecDeque<(bool, String)> =
    std::collections::VecDeque::with_capacity(self.stuck_window);
```

And the window-pop logic:
```rust
// Before:
if recent_errors.len() == self.stuck_window {
    recent_errors.pop_front();
}

// No change needed — pop_front() works on (bool, String) tuples too
```

### 2. Tests

Update the existing `stuck_detection_window_and_rate` and
`stuck_detection_partial_errors_below_threshold` tests in `src/run_core.rs` to use
the new `(bool, String)` tuple type.

Add a new test `stuck_detection_reports_most_repeated_tool`:
- Window of 4, threshold 0.75
- Errors pattern: `("tool_a", err), ("tool_b", err), ("tool_a", err), ("tool_b", ok)`
  → error_count=3, rate=0.75, most repeated is `tool_a` (appears 2 times in errors)
- Verify the logic selects `tool_a` not `tool_b`

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean  
- `cargo fmt --all` clean
- New test `stuck_detection_reports_most_repeated_tool` passes
- Existing stuck-detection tests pass (may need minor updates for tuple type)

## Notes for the agent

- Only `src/run_core.rs` needs editing — the `FinishReason::Stuck` type in
  `src/agent/types.rs` is unchanged (already uses `repeated_call: String`).
- The change is purely mechanical: `bool` → `(bool, String)` in the deque.
- Watch for any `VecDeque<bool>` literals in inline tests that need updating.
- **DO NOT modify** any file outside `src/run_core.rs`.
