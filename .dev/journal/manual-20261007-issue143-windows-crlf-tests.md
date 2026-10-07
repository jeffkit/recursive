# Manual landing of issue #143 — the last two windows-latest lib reds

- issue source: #143 (scene gap report, P3, okguitar)
- mode:        orchestrator-direct (Flowcast pipeline-143 worktree)
- baseline:    ddc38999 (#142 merged); worktree branch `v2-pipeline-143-1007070650`
- verdict:     completed

## Reported symptom

windows-latest, `cargo test --workspace` (run 37536218539 / job 112517702751:
2947 passed / 2 failed), both in the **lib** target — which fail-fasts and
stops the integration targets (issue51 re-verification) from running:

1. `knowledge::session_query::tests::normalize_falls_back_to_lexical_for_missing_paths`
   (`src/knowledge/session_query.rs:767`):
   `left: "D:\\definitely\\does\\not\\exist\\anywhere"` vs
   `right: "/definitely/does/not/exist/anywhere"`.
2. `runtime::tests::drive_turn_creates_instrumented_correlated_span`
   (`src/runtime/tests.rs:2799`): ``drive_turn must create an `agent.turn` span``.

The report hypothesised (2) was "the global tracing subscriber being clobbered
by a neighbouring test". That is impossible: the assertion is
`include_str!("../runtime.rs").contains(<multi-line literal>)` — a
compile-time string match, untouched by any concurrent test. The real cause is
below.

## Root causes

**(1) Non-platform-absolute expectation.** `normalize` (`session_query.rs:107`)
canonicalises when the path exists and *lexically absolutises* otherwise. A
POSIX `/…` literal is **rooted but not absolute on Windows** (no prefix), so
`path.is_absolute()` is false and the fallback joins the literal onto the
current drive → `D:\definitely\does\not\exist\anywhere` (separators
normalised to `\`), which can never equal `PathBuf::from("/definitely/…")`.
Windows-only by construction.

**(2) CRLF checkout vs. a multi-line `include_str!` match.** The repo has no
`.gitattributes`; the windows-latest checkout is CRLF (git autocrlf), and
`include_str!` embeds the file bytes verbatim. The first assertion of the span
test matches a two-line pattern:

```rust
src.contains(r#"info_span!(
            "agent.turn","#)
```

On CRLF the embedded text is `info_span!(\r\n            "agent.turn",`, so the
match fails; the test's other three assertions are single-line and pass. The
sibling assertions of the same kind in `src/http/mod.rs`
(`sessions_rebind_their_own_registry_in_container_tier`,
`session_rebind_reapplies_allow_tools_in_container_tier`,
`session_rebind_reattaches_mcp_tools`) already normalise with
`.replace("\r\n", "\n")` — this test was simply missed when it landed with #117,
which is why the red traces back to `2970bca3` and spans several commits.

Evidence (byte-level, no Windows needed):

```
$ python3 -c 'old = "info_span!(\n            \"agent.turn\""; lf=open("src/runtime.rs").read(); crlf=lf.replace("\n","\r\n"); print(old in lf, old in crlf, old in crlf.replace("\r\n","\n"))'
True False True
```

## What landed

Test-only change, two files:

- `src/knowledge/session_query.rs` — `normalize_falls_back_to_lexical_for_missing_paths`
  now builds its three paths from a `cfg!(windows)` triple
  (`D:\definitely\…` / `D:\definitely\elsewhere` vs. the POSIX literals), so the
  input is genuinely absolute on both platforms and the `..`-climbing guard is
  exercised with the same semantics. Both `assert_eq!`s gained a message
  (`"a missing absolute path must be passed through lexically"` /
  `"`..` in a missing absolute path must be resolved lexically"`) so a future
  platform drift names the property instead of dumping two paths.
- `src/runtime/tests.rs` — `drive_turn_creates_instrumented_correlated_span`
  reads `include_str!("../runtime.rs").replace("\r\n", "\n")`, with the same
  comment the `src/http/mod.rs` precedents carry.

No product code touched: `normalize`'s behaviour is correct on both platforms
(it is the *test* that hardcoded a POSIX path), and the span wiring is correct
(it is the *test* that assumed LF checkout).

## Audit of the same class (why no third red)

Every `include_str!`-based source assertion in the workspace was swept for
multi-line patterns (`contains` / `split` / `find` / `starts_with` /
`ends_with` with `\n`):

- normalised already: `src/http/mod.rs` (×5), `src/http/handlers.rs` (via
  `http/mod.rs`); the `tests/issue50_*.rs` doc assertions use single-line
  patterns.
- remaining `.contains("…\n…")` hits (`transcript.rs`, `config_file.rs`,
  `deliverables/*`, `compact/prompt.rs`, …) match *runtime output*, not
  checked-out source, so checkout line endings cannot affect them.
- `src/runtime/tests.rs:2818` (the one remaining multi-line source pattern)
  is inside the very test fixed here and is normalised by the same binding.

## Gates (macOS, worktree)

- `cargo clippy --workspace --all-targets --all-features -- -D warnings` —
  clean, exit 0 (cold ~23 min).
- `cargo fmt --all` — no residual diff.
- `cargo test --lib -- normalize_falls_back_to_lexical_for_missing_paths drive_turn_creates_instrumented_correlated_span`
  — 2 passed.
- `cargo test -p recursive-agent --lib` — **3028 passed; 0 failed**.
- `cargo test --workspace` — exit 0 (every target green).

The Windows leg itself can only be confirmed by the next `windows-latest` run;
`continue-on-error` for that matrix row stays (goal-410 territory).
