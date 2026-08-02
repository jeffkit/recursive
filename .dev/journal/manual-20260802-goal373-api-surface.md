# Manual edit: goal-373 — Tighten public API surface (non_exhaustive + pub(crate) flips)

**Date**: 2026-08-02
**Goal**: API hygiene — pre-1.0 kernel leaks internal types and lacks non_exhaustive.

## What changed (visibility/attribute only — no behaviour change)

### 1. `#[non_exhaustive]` added to four growing public enums
- `src/error.rs:12` — `Error` (the crate's primary error type; every new variant is
  currently a breaking change for external matchers).
- `src/permissions/mod.rs:29` — `DecisionReason`
- `src/permissions/mod.rs:242` — `RuleSource`
- `src/permissions/mod.rs:589` — `RuleBehavior`

Skipped per scope: `CompactionSkipReason` (`src/event.rs`) and `StreamChunk`
(`src/llm/chat.rs`) — forward-looking only, not worth match-arm churn. **Follow-up.**

### 2. `pub mod` → `pub(crate) mod` (verified zero external references)
- `src/lib.rs:22` — `atomic` (used only by `src/team.rs` via `crate::atomic::atomic_write`)
- `src/lib.rs:56` — `skills_injector` (internal skill-injection helper)
- `src/lib.rs:60` — `team` (used only by `src/tools/team_create.rs` / `team_delete.rs`,
  both behind `#[cfg(feature = "coordinator-mode")]`)

Re-grepped `crates/ tests/ examples/ e2e/ scripts/ docs/` — 0 hits for all three, so no
external caller exists to break. `cargo build --workspace` stayed green after each flip.

### 3. `ToolRegistry` fields → `pub(crate)`
- `src/tools/registry.rs` — `headless`, `hook_runner`, `auto_classifier` (all three now
  `pub(crate)`, matching their siblings). No external field reads exist; configuration goes
  through `with_headless` / `set_headless` / `with_hook_runner` / `with_auto_classifier`.

### 4. Trimmed over-broad `pub use` re-exports (quick, ≤4 lines)
- `src/lib.rs:98-101` — dropped `coordinator_system_prompt` and `MemoryEntry` from
  `pub use multi::{...}`. Kept `AgentMessage/AgentPool/AgentRole/MessageBus/MessageType/
  SharedMemory` — those ARE used by `tests/agent_team_integration.rs`.
- `src/lib.rs:148` — dropped `ExitStatus` from `pub use tools::{...}`.

## Match-arm fallout (non_exhaustive)

`#[non_exhaustive]` is **invisible within the defining crate** — so only external crates
(`crates/*`, `tests/`) need `_` arms. `cargo check --workspace --all-targets --all-features`
after the enum changes was **clean** with zero errors, meaning no external exhaustive match
existed. Surveyed the external `Error`/permissions usages anyway:
- `crates/recursive-tui/src/runtime_builder.rs:77,82,108` + `backend.rs:1274,1355` —
  **construct** `Error::Config` / `Error::Internal` struct variants → construction is
  unaffected by enum-level `#[non_exhaustive]` (confirmed: unit-variant construction of
  `Error::Cancelled` also still works externally, per new test).
- `tests/integration.rs:1000` — match already has `other => panic!` catch-all.
- `tests/http.rs` + `tests/integration.rs` — all other sites use `matches!` (auto `_ => false`).

## Tests added

NEW `tests/api_surface.rs` (7 tests) — external-crate contract tests that lock the
downstream API surface this goal tightened:
- `error_struct_variants_remain_constructible` — TUI pattern (`Config`/`Internal`/`Tool`).
- `error_unit_variant_remains_constructible` — `Error::Cancelled` constructible externally.
- `error_permission_denied_carries_decision_reason` — `tests/http.rs:259` pattern.
- `error_matches_with_catch_all_arm` — external `_` catch-all match contract.
- `decision_reason_construct_and_match_with_catch_all`.
- `rule_source_variants_construct_and_match_with_catch_all`.
- `rule_behavior_variants_construct_and_match_with_catch_all`.

These also satisfy the `agent-test-presence` gate (src/ changed → test-bearing change in the
same commit). They are contract tests (pass before & after the change; fail/refuse-to-compile
if a public path or constructibility is removed).

## Notes

- **Dead-code warnings in default-feature builds (13, all in `src/team.rs`):** after the
  `pub(crate) mod team` flip, the compiler no longer treats the module's `pub` items as
  externally reachable, and since `team_create`/`team_delete` are behind
  `#[cfg(feature = "coordinator-mode")]`, a default-feature build reports the module items
  as never-used. This is an expected consequence of the visibility flip, not a regression:
  the clippy gate runs `--all-features` (where coordinator-mode is on and the items are
  used) and is clean. Left as-is per "visibility/attribute only, don't expand scope";
  do NOT add blanket `#[allow(dead_code)]` to `src/team.rs` without a follow-up goal.
- **Mutation gate:** enumerated `cargo mutants` on the four changed src files (167 mutants
  total: error.rs 11, permissions/mod.rs 70, registry.rs 83, lib.rs 3). cargo-mutants 27.1.0
  generates **no attribute mutants** (no `#[non_exhaustive]` removal, no visibility mutants),
  so the new attributes add zero new mutation points. All 167 are pre-existing function-body
  mutants that the existing test suite is expected to cover (baseline-clean). Verified
  `src/error.rs` (11/11 caught, exit 0). Background run for the remaining 156 launched
  in-session; results appended below if they finished.
- **⚠ agent-mutants gate timeout risk:** measured per-mutant cost ≈23s (test suite with the
  gate's FEATURES, filtered per mutant). 167 mutants ⇒ ~60-75 min total, which exceeds the
  flow's agent-mutants gate timeout (`gates.json`: 2400000ms = 40 min; `spawnCapture` hard-
  kills at timeout). The gate auto-detects exactly the files this goal touches, so this goal
  is the first whose scoped mutant run exceeds the gate budget. If the flow gate times out it
  will enter resume-fix; a fix agent may need to run `agent-mutants.sh --jobs N` (copy mode)
  or split the run. Consider raising the agent-mutants gate timeout or documenting the
  expected runtime for multi-file goals. (Cannot change `.dev/flows/` per scope.)
- Follow-ups noted (out of scope): the 11 `src/tools/*` sub-modules still `pub mod` with zero
  external refs (need per-module care re: `Tool` trait-object reachability); `CompactionSkipReason`
  / `StreamChunk` non_exhaustive.
