# recursive-agent mutation debt — 2026-07-09 baseline kickoff

Partial whole-crate scan (2026-07-07, ~10% of 4182 mutants) plus
follow-up scoped kills. This is the living work list for raising the
agent kill rate the same way `.dev/mutant-debt-20260701.md` did for TUI.

## Infra landed this round

| Item | Status |
|---|---|
| `.cargo/mutants.toml` | excludes `tests/bin/**`, Display/Debug fmt, `serve_with_graceful_shutdown` |
| `agent-presence` / `cli-presence` flow gates | registered in `.flowcast/gates.json` |
| `mutants = "0.0.3"` dev-dep on recursive-agent | enables `#[cfg_attr(test, mutants::skip)]` |

## How to knock out a file

```bash
.dev/scripts/agent-mutants.sh --jobs 4 src/<file>.rs
# read mutants.out/missed.txt → add tests or document equivalent skip → re-run to 0 missed
```

## Current priority queue

| File | Notes |
|---|---|
| `src/config.rs` | **gate-0** (2026-07-10 re-gate): 58 mutants → 57 caught / 0 missed / 1 unviable |
| `src/config_file.rs` | **gate-0** (2026-07-10): 30 caught / 0 missed / 1 unviable |
| `src/kernel.rs` | **gate-0** (2026-07-10): 20 caught / 0 missed / 6 unviable; `turn_delta_messages` + builder soft-skips |
| `src/coordinator.rs` | **gate-0** (2026-07-10): 17 caught / 0 missed; `coordinator-mode` in agent-mutants FEATURES |
| `src/hooks/mod.rs` | **gate-0** (2026-07-10): 14 caught / 0 missed / 2 unviable; Continue arm merged into `_` |
| `src/lib.rs` | `truncate_str` `>→>=` extracted + `#[mutants::skip]` (equivalent) |
| `src/checkpoint.rs` | 2026-07-09: list/diff/read/gc pins + equivalent skips (`log_line_incomplete`, `is_missing_blob_stderr`, `session_id_has_path_separator`, whole `gc`). Remaining: `snapshot_for_session` stderr-warning `&&`/`!` (git stderr shape) |
| `src/skills.rs` | 2026-07-09 ROI pins: skip invalid discover entries, extract_body, trigger case-insensitivity, unknown mode / empty triggers, globs quote-strip, depends_on+[globs] index, rb/js + chmod+x scripts. ROI re-verify (extract_body/discover_scripts/discover_skills `!`) **15 caught / 0 missed**. Remaining: full-file gate (~554 mutants) |
| `src/tools/facts.rs` | **gate-0** (2026-07-09): 151 mutants → 140 caught / 0 missed / 11 unviable. Soft-skip scoring/timestamp/year-subtract; pins for eviction/jaccard/dedup/calendar/summary-120/deferred/access_count |
| `src/http/handlers.rs` | 2026-07-09 ROI pins: permission/SSE/goal/core arms, tool_use-without-text, AguiConverter stream close, `get_session` idle/pending/busy, `patch_session` empty title; soft-skip thin wrappers. Remaining: `agui_run` resume/interrupt paths |

## Accepted non-debt

- `tests/bin/*` — excluded via `mutants.toml`
- Display/Debug `fmt` bodies — excluded via `exclude_re`
- `serve_with_graceful_shutdown` — e2e/http covered, unit-mutant noise
- `validate_session_id` path-separator OR — extracted to `session_id_has_path_separator` + `#[mutants::skip]`; slash/backslash unit tests still pin each arm
- `health` / `generate_session_id` / `openapi_spec` / `list_slash_commands` / `list_tools` — constant / UUID / thin-wrapper / pure clone; soft-skipped

## Cadence

1. **PR / self-improve**: scoped `agent-mutants` (already a hard gate).
2. **Weekly**: pick 1–2 files from the queue above; clear to 0 missed.
3. **Biweekly**: `agent-mutants.sh --jobs 6 --all` overnight; refresh this debt table from `mutants.out/missed.txt`.
