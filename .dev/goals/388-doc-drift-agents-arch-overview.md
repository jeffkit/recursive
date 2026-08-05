# Goal 388 — Fix doc drift: AGENTS step budget + architecture overview paths

**Roadmap**: Doc accuracy (follow-on to Goal 369). Two high-traffic docs
still contradict the code; self-improve agents and humans both read them.

**Design principle check**:
- Implemented as: documentation-only edits (+ optional one-line comment
  fixes). No production behaviour change.
- ❌ Does NOT change `max_steps` defaults in code.

## Why (verified 2026-08-04)

1. **`AGENTS.md:93`** (repo root, injected into system prompt via
   `load_project_context`): "Step budget: default 200 (hard cap 400 with
   auto-resume)."
2. **`src/config.rs:904-918`** — `default_max_steps_is_unlimited` asserts
   default `max_steps == 0` (unlimited). README correctly documents `0`.
3. **`docs/architecture/overview.md:20`** — component map still shows
   `CLI (src/cli/)   TUI (src/tui/)` — both live under `crates/` since
   Goal 226.

## Scope (do exactly this, no more)

### 1. `AGENTS.md` — step budget paragraph

Replace the "default 200" claim with three distinct contracts:

- Product default: `RECURSIVE_MAX_STEPS=0` means unlimited (the run still
  stops on `NoMoreToolCalls`, stuck detection, transcript limit, cancel, or
  wall clock).
- Operator ceiling: `RECURSIVE_HARD_STEP_CAP`, when set, clamps the effective
  budget.
- Supervisor convention: self-improve / Flowcast may inject a finite
  `--max-steps` or environment value for a particular run; that is not the
  binary default.

Keep the stuck-detection and finish-reason sentences intact. Do not teach the
agent that every run is practically unlimited when a supervisor supplied a
finite cap.

### 2. `docs/architecture/overview.md` — entrypoint paths

Update the ASCII component map and any "Key Source Files" rows to:

- CLI → `crates/recursive-cli/`
- TUI → `crates/recursive-tui/`
- HTTP → `src/http/` (unchanged)

Bump the doc `timestamp` / "Last updated" if the front matter has one.

### 3. Repository-wide drift inventory

Before editing, run a read-only search for `src/cli/`, `src/tui/`, and
`default 200` across `AGENTS.md`, `.dev/AGENTS.md`, and `docs/`.

- Fix the two required high-traffic locations in §§1–2.
- Fix other occurrences only when they are the same unambiguous path/default
  claim and the document is still authoritative.
- For every occurrence intentionally left alone (historical review, archived
  handoff, or a larger stale document), list `file:line`, why it was not
  edited, and the proposed follow-up in the journal. Do not use an arbitrary
  occurrence-count cutoff.

## Files NOT to touch

- `src/config.rs` defaults.
- `.dev/AGENTS.md` invariants (unless it also repeats "default 200" —
  then fix that one line too).
- Product code.

## Acceptance

- `rg 'default 200' AGENTS.md` → empty.
- `rg 'src/cli/|src/tui/' docs/architecture/overview.md` → empty.
- `rg 'RECURSIVE_MAX_STEPS|unlimited|HARD_STEP_CAP' AGENTS.md` → present.
- Journal contains the repository-wide search command and an explicit list of
  every remaining occurrence, classified as historical, intentionally
  unchanged, or follow-up.
- No need for `cargo test` beyond what you already run; docs-only.
- Journal: `.dev/journal/manual-20260804-goal388-doc-drift.md`.

## Notes for the agent (traps)

- Root `AGENTS.md` is the **runtime** contract injected into prompts —
  wrong numbers teach the model false constraints ("don't burn the 200
  step budget") while the process may actually be unlimited.
- Do not reintroduce "200" as a soft guideline unless you also document
  it as a supervisor convention, not a binary default.
