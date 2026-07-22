# Manual edit: tui-auto-compaction

**Date**: 2026-07-21
**Goal**: Enable automatic transcript compaction in the TUI runtime so long interactive sessions compact at the same threshold the CLI does, instead of growing unbounded (only manual `/compact` was available before).
**Files touched**:
- `crates/recursive-tui/src/runtime_builder.rs` — added `build_compactor` / `build_compactor_from_env` helpers and wired `.compactor(...)` into both builder chains (`build_runtime` and `build_runtime_with_skill_tx`); added 4 unit tests.
**Tests added**:
- `build_compactor_disabled_when_threshold_zero`
- `build_compactor_uses_explicit_threshold_when_set`
- `build_compactor_rejects_non_positive_explicit_threshold`
- `build_compactor_auto_computes_when_env_unset`
**Notes**:
- Mirrors the CLI contract in `crates/recursive-cli/src/cli/builder.rs`: `RECURSIVE_COMPACT_THRESHOLD` env var — `0`/`off`/`false` = disabled, unset = auto-compute from model context window, positive int = explicit char threshold. Token threshold always derived via `default_compact_threshold_tokens(model)` and takes priority over the char estimate when real `prompt_tokens` are reported (better for CJK).
- The pure decision logic is split into `build_compactor_from_env(raw, model)` so it can be unit-tested without touching the process environment (avoids races under parallel `cargo test`).
- Impact analysis (GitNexus, upstream) on `build_runtime_with_skill_tx`: LOW risk, blast radius contained to `build_runtime_for_tui` → `Backend::spawn`.
- Quality gates: `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --workspace` all clean; `.dev/scripts/tui-test-presence.sh` PASS.
- Behaviour change is additive: when the env var is unset the TUI now auto-compacts where before it never did. Users who relied on the old no-compaction behaviour can set `RECURSIVE_COMPACT_THRESHOLD=0`.
