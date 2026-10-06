# Issue #111 — session failure reason had no carrier field (6 finish reasons → `Crashed`)

- issue:        #111 (P0, gap report from okguitar)
- baseline:     origin/main = fc29f3d4
- verdict:      completed

## Problem

`SessionStatus` collapses `BudgetExceeded` / `ProviderStop(_)` / `Stuck` /
`TranscriptLimit` / `PermissionDenialLimit` / `WallClockExceeded` into a single
`Crashed`, and `SessionMeta` persisted neither a `finish_reason` nor the error
text — so a session killed by the step budget was indistinguishable on disk
from a provider 400. `src/http/agui.rs` additionally matched the `Err` arm with
`Err(_)`, dropping the error string, and `ExportedTranscript` carried only the
coarse status.

## What landed

- `SessionMeta` gains `finish_reason: Option<String>` (canonical
  `FinishReason::Display` string) and `error: Option<String>` (failure text
  when the run ended with `Err`). Both `#[serde(default,
  skip_serializing_if = "Option::is_none")]`, so pre-existing `.meta.json`
  files load unchanged and no `schema_version` bump is required.
- `SessionStatus::for_finish(reason) -> (SessionStatus, Option<String>)` — the
  exhaustive match now yields the reason string for the six `Crashed`
  variants, `None` for `Completed` / `Interrupted`.
- `SessionWriter::finish_with_details(status, finish_reason, error)`; `finish`
  is the shorthand that clears the detail. The values **replace** what is on
  disk, so a resumed-and-completed session cannot keep a stale reason.
- AG-UI: `RunRecord` carries `finish_reason` / `error`; `src/http/agui.rs`
  threads them from `for_finish` / the `Err` text (no more `Err(_)` swallow);
  `apply_resume_tool_results` passes the existing detail through.
- CLI: `finish_to_session_status` now delegates to the session crate's
  exhaustive mapping (removing a second `_ => Crashed` table) and returns the
  pair; all four finalize sites (`run_once`, `run_loop` + its `Err` arm,
  `resume`) pass reason/error to `finalize_session_writer`.
- `ExportedTranscript` carries `finish_reason` / `error` too.
- Docs: `.meta.json` section of `docs/architecture/sessions.md`.

## Tests added

- `session::writer::tests::finish_with_details_persists_finish_reason`
  (acceptance: `budget_exceeded` readable from `.meta.json`, and a later plain
  `finish` clears it)
- `session::writer::tests::finish_with_details_persists_error_text`
- `session::tests::export_carries_finish_reason_and_error`
- `session::tests::for_finish_maps_exhaustively` extended to assert the reason
  string for all six failure variants (+`WallClockExceeded`)
- `cli::output::tests::finish_to_session_status_maps_errors_to_crashed_with_reason`

## Notes

No new dependencies. Runtime visibility (Langfuse traces) is #124, not this
change.
