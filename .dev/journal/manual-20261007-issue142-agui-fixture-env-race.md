# Manual landing of issue #142 — agui interrupt fixture resolved its session dir twice, unpinned

- issue source: #142 (scene gap report, P3, okguitar)
- mode:        orchestrator-direct
- baseline:    3f11c4d7 (worktree HEAD d4df0af0)
- verdict:     completed

## Reported symptom

windows-latest, `cargo test --workspace`:

```
http::agui::tests::prepare_run_interrupt_before_conflict_without_resume
  (src/http/agui.rs:1664, panic at :1669:22)
expected conflict, got Ok(PreparedAguiRun { goal: "hi", seed_transcript: None })
```

twice at the same point on the same commit (run 37407054881 job 112086829986 and
its rerun job 112089994488), while the previous commit was green — so not
introduced by the intervening change. The report hypothesised that a transient
`.interrupts.json` write failure (`save_open_interrupts` is a silent
`atomic_write(..).ok()`) was the cause.

## Root cause — the session dir is resolved twice, and its base is process env

`prepare_run` derives the thread's session dir through
`http::agui::agui_session_dir` → `agui_session::resolve_session_dir` →
`session_dir` → `paths::user_sessions_dir`, whose base is **process env**:

- `RECURSIVE_SESSIONS_DIR` if set (hard override), else
- `RECURSIVE_HOME` (or `$HOME`)/`workspaces/<ws-hash>/sessions`.

The pre-#142 test resolved that dir **twice** with env-sensitive calls: once to
write the fixture, once inside `prepare_run` to read it — while *not* holding
`test_util::env_lock()`. Every sibling test in the same binary that redirects
those vars (the three `pinned_home()` tests in this very module, plus every
`PinnedRecursiveHome` test elsewhere, plus the `paths`/`config`/`handlers`
session-dir tests) can flip the base between the two resolves; the read then
looks under a *different* base, sees no fixture, and reports "expected conflict,
got Ok". `test_util`'s own module docs call out exactly this hazard: "Tests that
only *read* env-derived state also need to hold it to avoid observing a
torn-down tempdir that some other test had pointed `RECURSIVE_HOME` at."

Windows-only in CI because the vulnerable window is the fixture write itself
(create + `sync_all` + rename, with Defender/AV inspecting the new file) — tens
of ms there vs sub-ms on macOS/APFS, i.e. a much higher hit rate. Nothing in
the product path is Windows-specific, and the reproduction below shows the
failing side is the **read** (the write does land), not the report's
"silent write swallow".

## Reproduction (before/after, same process, same hostile env)

Temporary scratch tests (added, run, then removed — not part of the landing):

- `zz_race_probe_env_flip`: holds `env_lock` and re-points `RECURSIVE_HOME`
  between two live tempdirs every ~200 µs for 3 s — i.e. exactly what the
  `pinned_home()` siblings do, at worst-case frequency.
- `zz_race_probe_fixture_unpinned`: the pre-#142 test body, verbatim.
- `zz_race_probe_fixture_pinned`: the same body with the fix.

```
$ BIN=<lib unit-test binary>; for i in $(seq 1 10); do $BIN zz_race_probe --test-threads=3; done
unpinned failures: 8/10
pinned   failures: 0/10
```

Failure text of the unpinned body, byte-identical to the CI report:

```
thread 'zz_race_probe_fixture_unpinned' panicked at src/http/agui.rs:2060:22:
expected conflict, got Ok(PreparedAguiRun { goal: "hi", seed_transcript: None })
```

## What landed

`src/http/agui.rs` (test module only, no product code):

1. `write_interrupts` now reads its own fixture back and asserts it — a failed
   write says "interrupt fixture … not written / did not round-trip" instead of
   surfacing later as the misleading "expected conflict, got Ok" (issue
   acceptance §2).
2. The four session-dir-touching `prepare_run_*` tests
   (`prepare_run_interrupt_before_conflict_without_resume`,
   `prepare_run_resume_without_prior_run_is_bad_request`,
   `prepare_run_resume_must_cover_all_open_interrupts`,
   `prepare_run_resume_with_no_open_interrupts_is_bad_request`) now use
   `test_util::IsolatedWorkspace`: one tempdir owns `RECURSIVE_HOME` **and**
   `RECURSIVE_SESSIONS_DIR` and the global env lock is held for the test's
   lifetime, so the write base and the read base are provably the same. Same
   idiom the AG-UI neighbours already use (`src/agui_session.rs`,
   `tests/agui_e2e.rs`).

## Not changed (and why)

- `save_open_interrupts` / `load_open_interrupts` keep their best-effort
  semantics: the evidence shows the store is written correctly here, so adding
  retry/error handling to the product would be unjustified. The test-side
  read-back assertion is what makes a future real write failure observable.
- No other test in the crate resolves an env-derived session dir without the
  lock in a write-then-read shape; `tests/agui_e2e.rs` already pins both vars
  under the lock (checked).

## Gates (macOS, worktree)

Env used: `RECURSIVE_SESSIONS_DIR` unset + a temp `RECURSIVE_HOME`, i.e. the CI
condition. (The repo's own self-improve pipeline exports a shared
`RECURSIVE_SESSIONS_DIR` instead, which — being the hard override — hides this
whole class of race from local runs. Unset it when reproducing.)

- `cargo test -p recursive-agent --lib` — **2997 passed; 0 failed** (the
  binary now holds ~3000 tests; the issue quotes 2823 from that older commit,
  the assertion is that the *whole* lib target runs green, not the count).
- `cargo test -p recursive-agent --lib http::agui::` x40 — **0 failures**
  (the module's own env-flipping `pinned_home()` siblings run concurrently).
- `cargo test --workspace` — exit 0, every target green, including
  `-p recursive-agent --test agui_e2e` (**9 passed**) which exercises the
  `.interrupts.json` store end to end.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` —
  clean (exit 0).
- `cargo fmt --all -- --check` — clean.

The Windows leg itself can only be confirmed by the next `windows-latest` run;
`continue-on-error` for that matrix row stays (goal-410 territory).
