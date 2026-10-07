# Manual edit: issue138-otlp-v4-keys

**Date**: 2026-10-06
**Goal**: #138 — OTLP 属性补 v4 原生映射键（sessionId / cost 只落 metadata，
Sessions 分组与成本列不填充）。

## What was done

The `#124` collector reported session as `langfuse.trace.sessionId` and cost as
`langfuse.observation.metadata.cost_usd`. In Langfuse **v4** those names land in
the metadata map only, so `events_full.session_id` stays empty (Sessions view
does not group) and `cost_details` stays `{}` (cost column/aggregates are 0).

Langfuse v4's native OTel mapping (per
https://langfuse.com/integrations/native/opentelemetry) wants:

- session: `session.id` (or `langfuse.session.id`)
- cost: `langfuse.observation.cost_details` (JSON) or `gen_ai.usage.cost`

The collector now **adds** (does not replace) both native keys on top of the
existing plaita-compatible `langfuse.trace.*` / `cost_usd` attributes:

- `session.id` on the root and every generation record (same value as
  `langfuse.trace.sessionId`);
- `langfuse.observation.cost_details` = `{"total": <usd>}` on the root (run
  total) and every generation (per-step cost), as a JSON string.

## Files touched

- `src/observability/collector.rs` — new `ATTR_SESSION_ID` /
  `ATTR_OBSERVATION_COST_DETAILS` constants, `cost_details_json` helper
  (`serde_json`, already a dependency), and the new attributes on
  `root_record` / `step_record`.
- `.dev/scripts/agent-mutants.sh` — add `otel` to the mutants `FEATURES`.

## Tests added

`src/observability/collector.rs`:

- `v4_native_session_and_cost_keys_populate_grouping_and_cost` — root and step
  carry `session.id`; `cost_details.total` (parsed as JSON) equals the root
  `cost_usd` and is non-zero.
- `empty_session_omits_both_session_keys` — an empty session id emits no
  `session.id` key.
- `cost_details_is_json_with_a_numeric_total` — pins the exact JSON shape and
  a numeric `total`.

## Verification

- `cargo test -p recursive-agent --features otel --lib observability` — 46
  passed (incl. the three new tests).
- `cargo clippy -p recursive-agent --all-targets --features otel -- -D
  warnings` — clean; `cargo fmt --all -- --check` clean.
- `bash .dev/scripts/agent-mutants.sh --jobs 3` — `--in-diff` found the 5
  mutants in `collector.rs::root_record` / `step_record` / `cost_details_json`
  and the **unmutated baseline passed** with `otel` in the feature set (the
  full run was stopped early — the box hit 100% disk with three pipelines
  building; the two `Observation::default()` mutants are already recorded
  `unviable` since `Observation` has no `Default` impl).

## Notes

- The `agent-mutants` `FEATURES` set did not include `otel`, so
  `src/observability/*` (all `#[cfg(feature = "otel")]`-gated) was never
  compiled during mutation runs. cargo-mutants discovers mutants by walking the
  AST, not the build graph, so every mutant in those files is a no-op and is
  reported **MISSED** — the gate would fail on any change to the observability
  module regardless of test quality. Adding `otel` makes those mutants actually
  compile and be exercised; it expands coverage, it does not shrink the gate.
- Conservative on purpose: the pre-existing `langfuse.trace.*` / `cost_usd`
  attributes are untouched so plaita's obs.py stack and recursive still produce
  comparable traces.
- `Attributes` are also attached to the root span (a `span`, not a
  `generation`); Langfuse ignores `cost_details` on non-generations, so the
  generation records remain the authoritative cost source.

## Resume run (2026-10-07)

Resumed from the WIP snapshot `wip-pipeline-138-1006121744` after the previous
attempt was preserved at the `test` gate. The preserved failure was **not** a
code defect: `cargo test --workspace` died with `rustc-LLVM ERROR: IO failure
on output stream: No space left on device` (the host disk filled while several
pipelines were building). No source change was needed — the collector change
and its tests were re-verified green on a fresh worktree (276 GiB free):

- `cargo fmt --all -- --check` — clean.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` —
  clean (6m34s cold).
- `cargo test --workspace` — all targets pass, including
  `observability::collector::tests::v4_native_session_and_cost_keys_populate_grouping_and_cost`,
  `empty_session_omits_both_session_keys` and
  `cost_details_is_json_with_a_numeric_total`. (The workspace test build enables
  `otel` through `crates/recursive-cli`'s dependency feature unification, so
  these run under plain `cargo test --workspace`, not only under an explicit
  `--features otel`.)
- `bash .dev/scripts/agent-mutants.sh --jobs 8` — `--in-diff` scoped the run to
  the 5 mutants in `root_record` / `step_record` / `cost_details_json`, and the
  unmutated baseline passed. **No mutant survived**: 2 caught (`delete !` in
  `step_record`; `cost_details_json` → `"xyzzy"`) and 2 unviable
  (`… -> Observation with Default::default()` — no `Default` impl). The host
  interrupted the run before the last (sibling) mutant finished, but the result
  is what the `agent-mutants.sh` change is for: with `otel` in the feature set
  these mutants actually compile, so the gate is no longer a blanket MISSED.

Verified `cargo mutants --list` enumerates `src/observability/collector.rs`
mutants **even without** `otel` in `--features` (cargo-mutants walks the AST,
not the build graph) — so the missing feature really did make every
observability mutant a no-op, exactly as claimed above.
