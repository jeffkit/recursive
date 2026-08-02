# Manual edit: goal-374 — Pin missing error/failure-path tests (atomic torn-write, OpenAI SSE parse)

**Date**: 2026-08-02
**Goal**: Add unit tests pinning two real failure paths that had zero coverage:
(1) `atomic_write` behaviour when a stale `.tmp-*` orphan (a crashed writer's
leftover) is on disk; (2) the OpenAI SSE parser's malformed-chunk error path.
Plus the optional Anthropic mirror test (harness identical → trivial add).

## Files touched

- `src/atomic.rs` — **test-only** (`#[cfg(test)] mod tests`).
  Added `test_atomic_write_ignores_stale_tmp_orphan`: pre-creates `target.txt`
  with `v1`, writes a stale `.tmp-target.txt-{pid+1}-999999` orphan matching the
  `TEMP_SEQ` naming pattern (`.tmp-{name}-{pid}-{seq}`) with a *different* pid/seq
  than the live writer, calls `atomic_write(target.txt, b"v2")`, then asserts the
  target is `"v2"` (not torn) and the stale orphan is **ignored** (still present,
  content untouched).
  - Pinned contract (read the code first): `atomic_write` never scans the parent
    dir for pre-existing tmp files; it only writes its own fresh temp and renames.
    So the orphan is ignored, NOT reaped. The test documents this explicitly and
    does NOT promise cleanup (a `cleanup_tmp_orphans` helper is a separate goal).
  - No production change.

- `src/llm/openai.rs` — **test-only** (`#[cfg(test)] mod tests`).
  Added `parse_sse_stream_returns_error_on_malformed_chunk`: mirrors the existing
  `stream_concatenates_sse_chunks` one-shot TCP harness, but serves a body whose
  FIRST line is `data: {not valid json` followed by a valid chunk and `data: [DONE]`.
  Asserts `provider.stream(...)` returns `Err(Error::Llm { .. })` whose message
  contains `"SSE parse error"`, wrapped in `tokio::time::timeout(5s)` so a parser
  hang fails the test instead of hanging the suite.
  - Exercises the full chain: `stream_inner` → `parse_sse_stream` →
    `process_sse_line` (the `serde_json::from_str(...).map_err(...)` at the
    `"SSE parse error"` format site).
  - No production change.

- `src/llm/anthropic.rs` — **test-only** (optional part 3; harness identical).
  Added `parse_sse_stream_returns_error_on_malformed_chunk` mirroring the OpenAI
  test with the Anthropic event framing. Load-bearing detail pinned: the
  malformed payload must follow an `event: message_start` line — a bare malformed
  `data:` line with no event name would fall into the "unhandled SSE event" debug
  branch and be silently ignored, which is NOT the real failure mode (the goal
  explicitly flags anthropic.rs:480/503/557/579 as untested).
  - No production change.

## Tests added

- `src/atomic.rs`: `test_atomic_write_ignores_stale_tmp_orphan`
- `src/llm/openai.rs`: `parse_sse_stream_returns_error_on_malformed_chunk`
- `src/llm/anthropic.rs`: `parse_sse_stream_returns_error_on_malformed_chunk`

## Verification

- `cargo test --lib` for each new test — pass (atomic 1/1; SSE filter 2/2 across
  both adapters).
- `cargo test --workspace` — running (bg); expected green.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — running (bg).
- `cargo fmt --all` — clean (no reformat needed).
- `git diff --stat` → 3 files, +188 lines, **all hunks inside `#[cfg(test)]`
  modules**; zero production-code changes → e2e gate correctly skipped
  (tests-only diff-scope short-circuit).
- `rg "ignores_stale" src/atomic.rs` → 1 hit (the new test).
- `rg "malformed_chunk" src/llm/openai.rs src/llm/anthropic.rs` → 1 hit each.

## Notes / judgment calls

- **Pinned actual behaviour, not idealised**: the atomic test asserts the orphan
  is IGNORED (present, untouched) because that is what `atomic_write` does today.
  The goal explicitly warned against asserting cleanup that doesn't happen.
- **Anthropic mirror included** (goal part 3): harness is byte-for-byte the same
  one-shot TCP + `provider.stream()` pattern, so it was a trivial add. The
  `event:`-line requirement makes it a slightly stronger pin than the OpenAI one
  (guards against the parser "fixing" malformed input by falling into the
  unhandled-event branch).
- **No bugs revealed**: neither test exposed a production defect; both parsers
  correctly return `Error::Llm` on malformed input, and `atomic_write` correctly
  leaves the target intact when a stale orphan is present. No follow-up goal
  needed for a bug; the only plausible follow-up is the optional
  `cleanup_tmp_orphans(parent)` reaper (out of scope, noted in the atomic test).
