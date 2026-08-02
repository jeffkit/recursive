# Goal 374 — Pin missing error/failure-path tests (atomic torn-write, OpenAI SSE parse)

**Roadmap**: Test coverage — real failure paths have zero pinning tests

**Design principle check**:
- Implemented as: NEW unit tests added to existing `#[cfg(test)]` modules in
  `src/atomic.rs` and `src/llm/openai.rs`. **Production code is not modified.**
- ❌ Does NOT touch the kernel, run loop, or any invariant. If a test reveals a real bug,
  STOP and report it (do not fix production here; a separate goal fixes it).
- No new deps (`tokio` already in use; the SSE test feeds bytes into the existing parser).

**This is a tests-only goal** — no `src/` production change → the e2e gate is skipped
(diff-scope short-circuit). Fast iteration.

## Why (the coverage gaps, with evidence)

Two real failure paths exist in production code but have **no test pinning them**:

### Gap 1 — `atomic_write` has no torn-write / stale-temp recovery test

`src/atomic.rs:1-17` (doc comment) promises:
> "a reader that observes `path` therefore sees either the old content or the full new
> content — never a half-written file."

Existing tests (`src/atomic.rs:93-170`): `creates_file`, `overwrites`,
`cleans_tmp_on_success`, `empty_bytes`, `nested_path`, `concurrent_writes_serialise_via_pid`,
`async_roundtrip`. **All test the SUCCESS path.** None simulate a crash *between*
`write_all` and `rename` — i.e. a leftover `.tmp-<name>-<pid>-<seq>` orphan on disk at
startup. The `TEMP_SEQ` design (`:35-39`) means a real crash leaves a recognisable orphan
that nothing reaps, and no test asserts (a) the original file is intact, (b) the stale temp
is harmless.

The property matters: `atomic_write` backs session meta, cost ledger, checkpoints — files
whose corruption is a data-integrity bug, not a transient.

### Gap 2 — OpenAI SSE parser's malformed-chunk error path is untested

`src/llm/openai.rs:625` (`parse_sse_stream`) + `:767` (`process_sse_line`). At `:787`:
```rust
message: format!("SSE parse error: {e}; data: {data}"),
```
The parser correctly returns `Err(Error::Llm { ... })` on a bad chunk — but `grep -n 'SSE parse error\|malformed\|bad.*chunk' src/llm/openai.rs` finds **no test** that feeds a malformed
`data:` line and asserts this error surfaces. Every streaming test feeds well-formed
`data: {...}`. A truncated/garbled chunk from a flaky proxy is the realistic failure mode
(equivalent Anthropic sites at `anthropic.rs:480/503/557/579` also have no failure test).

## Scope (do exactly this, no more)

### 1. `atomic_write` stale-temp / torn-write test (`src/atomic.rs` `#[cfg(test)]`)

Add `test_atomic_write_ignores_stale_tmp_orphan` (or similar):
- Pre-create `target.txt` with content `"v1"`.
- Manually write a stale temp file matching the `TEMP_SEQ` naming pattern
  (`src/atomic.rs:35-39` — read it for the exact `.tmp-<name>-<pid>-<seq>` shape). Use a
  *different* pid/seq so it doesn't collide with what the current writer would generate.
- Call `atomic_write(target.txt, b"v2")`.
- Assert: result content is `"v2"` (the new write succeeded), `target.txt` is not torn,
  and the *stale* orphan either (a) is ignored (still there — acceptable) or (b) is cleaned
  up (better). **Pin whichever the ACTUAL behaviour is** — read the code first to know
  whether `atomic_write` touches pre-existing tmp files (it probably doesn't, so expect
  (a): orphan ignored, target correct). Document the pinned contract in the test comment.

The test asserts the **data-integrity property**: a prior crash's leftover does not corrupt
the next write. If you find `atomic_write` *does* corrupt on a stale orphan (e.g. rename
collision), that's a bug — STOP, don't fix production here, report it (journal + a new goal).

### 2. OpenAI SSE malformed-chunk test (`src/llm/openai.rs` `#[cfg(test)]`)

Add `parse_sse_stream_returns_error_on_malformed_chunk` (or similar):
- Feed the parser a mock SSE body containing `data: {not valid json\n\n` followed by a
  valid terminal chunk (`data: [DONE]\n\n` or whatever the existing streaming tests use).
- Assert: the call returns `Err(Error::Llm { .. })` whose message contains
  `"SSE parse error"`.
- Read how the existing streaming tests construct their input (they feed well-formed
  `data: {...}` lines — mirror that harness but inject one malformed line). If the parser
  is fed via an `async_stream` / channel, mirror that; if via a `Vec<u8>` body, mirror that.

The test pins: a flaky proxy's garbled chunk surfaces as a diagnosable `Error::Llm`, not a
silent panic or hang. Add `tokio::time::timeout` around the await (per `.dev/AGENTS.md`
network-test discipline) even though this is a mock, to prevent a parser bug from hanging
the suite.

### 3. (Optional, only if trivial) Anthropic malformed-SSE test

Mirror the OpenAI test for `src/llm/anthropic.rs` if the harness is identical and it's a
5-minute add. Otherwise note as follow-up.

## Files NOT to touch

- `src/atomic.rs` production code (`atomic_write` / `atomic_write_async` / `TEMP_SEQ`).
- `src/llm/openai.rs` / `anthropic.rs` production code (the parsers).
- Any other module. `.dev/flows/`.

If a test reveals a bug, **do not fix the production code in this goal** — stop, report in
the journal, and a follow-up goal will fix it. This goal adds tests only.

## Acceptance

- `cargo test --workspace` green (the new tests pass).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.
- The new tests **fail on the absence of the behaviour they pin** — i.e. if you removed the
  `SSE parse error` formatting or the `atomic_write` tmp-handling, the test would break.
  Verify by reasoning (the test asserts on the specific error message / specific
  data-integrity outcome).
- Grep: `rg "stale_tmp_orphan|ignores_stale" src/atomic.rs` and
  `rg "malformed_chunk|SSE parse error" src/llm/openai.rs` each return ≥1 hit (the new tests).

## Notes for the agent (traps)

- **Pin actual behaviour, don't assert idealised behaviour.** If `atomic_write` does NOT
  clean up stale orphans (likely), the test asserts "orphan ignored, target correct" —
  NOT "orphan cleaned up". Read the code before writing the assertion. The point is a
  regression alarm, not a spec of desired behaviour.
- **`TEMP_SEQ` naming — match it exactly.** Read `src/atomic.rs:35-39` for the format. A
  stale temp with the WRONG pattern won't test the realistic case (the orphan from a real
  crash matches the pattern by construction).
- **Don't introduce a cleanup/reaper function.** If the orphan isn't cleaned, that's a
  follow-up (a "cleanup_tmp_orphans(parent)" helper is a separate goal). This goal just
  pins the current behaviour + the data-integrity guarantee.
- **SSE test must not hang.** Even with mock input, wrap in `tokio::time::timeout`. If the
  parser loops on malformed input, the test should fail on timeout, not hang the suite.
- **The malformed chunk must reach `process_sse_line` / `parse_sse_stream`.** Don't test a
  helper that doesn't exercise the real error path. Read the call chain
  (`stream_inner:536` → `parse_sse_stream:625` → `process_sse_line:767`) to know where to
  inject.
- **Mock harness:** `mockito = "1.7.2"` is a dev-dep (`Cargo.toml:127`) and `wiremock` is
  available in agui-client — but the SSE test probably doesn't need a server, just feeding
  bytes into the parser. Mirror whatever the existing OpenAI streaming tests use.
- **Tests-only → e2e skipped.** Confirm `git diff --stat` shows only `src/atomic.rs` and
  `src/llm/openai.rs` (and only the test modules). If you find yourself editing production
  code, you've drifted — stop and reconsider.
