# Manual edit: readme-plaita-engine

**Date**: 2026-09-30
**Goal**: Document the plaita engine form of the self-improve loop in README.md,
including the new launcher, the thin-flow/thick-engine split, and the flowcast
rollback path. Docs-only change (no code / scripts touched).

**Files touched**:
- `README.md` — new `## Self-Improving Agents` section (launcher usage,
  architecture table, flowcast rollback) + a Docs-list entry pointing at the
  website guide.
- `website/en/guide/self-improve.md` — "How it works" now leads with the plaita
  engine; added an "Engines: thin flow, thick engine" section with the
  plaita/flowcast comparison table.

**Tests added**: none (markdown only; no Rust or script changes)

---

## What the new section covers

- **Launcher** — `.dev/scripts/launch-flow-plaita.sh`, same core flag surface as
  `launch-flow.sh` (`--goal` / `--goal-file` / `--provider` / `--model` /
  `--run-id` / `--hitl` / `--no-review` / `--no-commit` / `--max-steps` /
  `--reviewer-provider`), background `nohup` run, prints run-id + log path,
  supervisor polls `.flowcast/runs/<run-id>/state.json`.
- **Architecture** — thin flow + thick engine: definition/observation belong to
  plaita-console, execution stays local. Node skeleton =
  `.dev/flows/self_improve_flow.py` (45 nodes, compiled to
  `self-improve.plaita.json`); engine logic = `.dev/flows/self_improve_engine.py`
  (each node is a shim that calls `engine step <name>`). Behaviour changes need
  only the engine; graph changes need `build_self_improve_flow.py` + a console
  publish.
- **flowcast path** — `.dev/scripts/launch-flow.sh` retained as the
  behaviourally equivalent rollback path (same gates, same verdicts, same
  run-dir / `state.json` contract).

## Verification

Docs-only diff; ran the three gates anyway per the working contract:
`cargo fmt --all -- --check` (clean), `cargo clippy --all-targets --all-features
-- -D warnings` (clean), `cargo test --workspace` (all green).

## Review fix (revision 2) — verdict contract corrected

The first revision listed the shared verdict set as
`committed / rolled-back / skip-commit / panic-preserved`. That was wrong:

- Both engines actually emit `committed` / `failed-preserved` / `skip-commit` /
  `panic-preserved` (`failed-preserved` is the canonical failure verdict — a
  failing gate preserves the worktree, it does not discard it).
- The plaita path never emits `rolled-back` at all (grep of
  `self_improve_flow.py` + `self_improve_engine.py` finds no such literal).
- `rolled-back` exists only in flowcast, at `self-improve.flow.js:922`, as a
  rare double-failure fallback (attempt error **and** `preserveScene` throws).

Fixed in both files, plus two nearby "rollback" phrasings that contradicted the
preserve semantics (the README intro and the guide's step 6). Remaining
"rollback" usages describe the flowcast *path* role, not a verdict.

