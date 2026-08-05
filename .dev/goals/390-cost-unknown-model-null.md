# Goal 390 — Unknown model cost must not look like $0.00

**Roadmap**: Observability honesty. When pricing is missing for a model,
`CostTracker` currently persists `cost_usd: 0.0`, which dashboards and humans
read as "free" instead of "unknown".

**Design principle check**:
- Implemented as: a small JSON-shape correction in `src/cost.rs` plus focused
  tests. The `cost_usd` key remains present; unknown pricing serializes as
  JSON `null`, while known costs remain numbers.
- ❌ Does NOT branch `run_core.rs::run_inner` or invent new pricing entries.
- ❌ Does NOT redesign cost files or SDKs.

## Why (verified 2026-08-04)

`src/cost.rs:165-170`, specifically line 168, updates session meta with:

```rust
self.cost_usd().unwrap_or(0.0)
```

`cost_usd()` already returns `None` for an unknown model. Converting that to
`0.0` destroys the distinction between missing pricing and a legitimate known
zero cost. `CostData::cost_usd` is already `Option<f64>`, so JSON `null` is the
natural representation.

## Scope (do exactly this, no more)

### 1. Fix the `.meta.json` contract

When updating session meta:

- Always keep the `cost_usd` key present.
- Insert a JSON number when `cost_usd()` is `Some(value)`.
- Insert `serde_json::Value::Null` when `cost_usd()` is `None`.
- Remove the unknown-model `unwrap_or(0.0)` fallback.
- Keep `.meta.json` consistent with direct `CostData` serialization: the
  unknown shape is one contract (`null`), not a mix of null and absent keys.

### 2. Audit narrow call sites

- Grep `cost_usd().unwrap_or` and relevant `unwrap_or(0.0)` uses in
  `src/cost.rs`; change only paths that collapse unknown pricing into zero.
- If an existing HTTP/TUI formatter directly consumes this optional value and
  currently renders unknown as `0.00`, update that single formatter to
  `"unknown"` / `"—"`. Do not broaden this goal into an observability UI
  redesign.

### 3. Tests

Extend the focused cost tests with all three JSON shapes:

1. unknown model → `.meta.json["cost_usd"] == Value::Null` and the key exists;
2. known priced model → `cost_usd` remains a JSON number;
3. a known fixture with legitimate zero cost → numeric `0.0`, not null.

Keep the existing finish/meta-write and `CostData` serialization round-trip
tests green. Add an explicit assertion that unknown direct `CostData`
serialization and `.meta.json` use the same null representation.

## Files NOT to touch

- Provider/pricing catalog files unless a test-only known-zero fixture cannot
  be constructed locally.
- Kernel loop, transcript format, or unrelated observability fields.
- Python/TypeScript SDK rewrites; record any downstream compatibility concern
  in the journal.
- `.dev/flows/` or `.flowcast/`.

## Acceptance

- `rg 'cost_usd\(\)\.unwrap_or\(0\.0\)' src/cost.rs` → empty.
- The unknown, known-number, and known-zero JSON-shape tests exist and pass by
  name.
- Existing `test_cost_tracker_finish_writes_files`, unknown-model, and
  serialization round-trip tests stay green.
- `cargo test -p recursive-agent cost_tracker` green (use the exact matching
  filters and record them in the journal).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  clean; `cargo fmt --all -- --check` clean.
- Journal: `.dev/journal/manual-20260804-goal390-cost-null.md` documents the
  key-present/null contract and any downstream reader found during the audit.

## Notes for the agent (traps)

- Do not turn every numeric zero into null. A known/free model with
  `Some(0.0)` must remain numeric zero.
- `serde_json::Number::from_f64` can reject non-finite values; preserve the
  existing handling for known numeric costs rather than coupling that concern
  to unknown pricing.
- Do not choose key omission: downstream readers should handle exactly
  number-or-null.
