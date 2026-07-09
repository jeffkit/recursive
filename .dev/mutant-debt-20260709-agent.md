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
| `src/config.rs` | OR-gate + 16KB cap tests added 2026-07-09; re-gate |
| `src/config_file.rs` | trailing-newline append test added; re-gate |
| `src/kernel.rs` | non-summary prepend test added; re-gate |
| `src/coordinator.rs` | non-`"1"` env rejection test added; re-gate |
| `src/hooks/mod.rs` | Continue short-circuit pin added; re-gate |
| `src/lib.rs` | `truncate_str` `>→>=` extracted + `#[mutants::skip]` (equivalent) |
| `src/checkpoint.rs` | 2026-07-09: list/diff/read/gc pins + equivalent skips (`log_line_incomplete`, `is_missing_blob_stderr`, `session_id_has_path_separator`, whole `gc`). Remaining: `snapshot_for_session` stderr-warning `&&`/`!` (git stderr shape) |
| `src/skills.rs` | 2026-07-09 ROI pins: skip invalid discover entries, extract_body, trigger case-insensitivity, unknown mode / empty triggers, globs quote-strip, depends_on+[globs] index, rb/js + chmod+x scripts. ROI re-verify (extract_body/discover_scripts/discover_skills `!`) **15 caught / 0 missed**. Remaining: full-file gate (~554 mutants) |
| `src/tools/facts.rs` | 2026-07-09: strengthened tokenize whitespace/punct split pin. ROI re-verify (tokenize/load/is_active) **9/9 caught**. Remaining: full-file scan (~200) |
| `src/http/handlers.rs` | 2026-07-09 ROI pins: permission/SSE/goal + core arms (PartialToken/ToolCall/Done/PlanProposed), tool_use-without-text, AguiConverter PartialToken→ToolCall stream close; soft-skip `health` / `generate_session_id` / `openapi_spec` / `list_slash_commands` / `list_tools`. Remaining: `get_session`/`patch_session` AppState fixtures, `agui_run` |

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
