# Manual fix: issue51 env test's host-shell arm was Unix-only (Windows os error 3)

- issue:        #141 (windows CI long-red, next layer under #139)
- baseline:     19e569e8
- mode:         orchestrator-direct (one cfg gate in one test)
- verdict:      completed

## Root cause (the reported diagnosis was wrong)

`tests/issue51_sandbox_env_inheritance.rs:135` failed on `windows-latest` with

```
Tool { name: "Bash", call_id: None, message:
  "C:\\Users\\RUNNER~1\\AppData\\Local\\Temp\\.tmpWwjkIL:
   The system cannot find the path specified. (os error 3)" }
```

That is **not** a broken/absent temp dir. Line 135 is arm 3 of the test — the
only arm that uses the *real* `LocalTransport`:

- `RunShell` (arm 3, no `cwd` arg) → `LocalTransport::exec_shell`
  (`src/tools/transport_layer/transport.rs:886`) → `Command::new("/bin/sh")`.
- On Windows `/bin/sh` is an absolute path; CreateProcessW resolves it against
  the current drive (`C:\bin\sh`), whose directory component does not exist →
  `ERROR_PATH_NOT_FOUND` = **os error 3**. No POSIX shell exists there, so the
  spawn fails before any command runs. (arms 1–2 use the in-process
  `CapturingTransport`; they never spawn anything and were already green.)
- The `<temp path>:` prefix in the message is a **red herring**: `RunShell`
  wraps transport errors with `tools::execution::fs::transport_io_error(&cwd, &e)`
  (`format!("{}: {e}", path.display())`), i.e. it prepends the *cwd* verbatim.
  The `TempDir` was created and alive; it only labels the error.

Product-side `/bin/sh` on Windows is a known, separately tracked gap — not this
P3 fixture-level fix. Repo convention for `/bin/sh`-driven tests on Windows is
already established: `.dev/journal/manual-20260603-fix-ci-windows-tests.md`
(`#[cfg_attr(target_os = "windows", ignore)]` for `runtime_falls_back_to_diff_for_run_shell`
and `backend.rs`), and `src/tools/execution/shell.rs:205` +
`src/tools/transport_layer/transport.rs:1045` gate their whole test modules
with `#[cfg(not(target_os = "windows"))]`.

## Change

`tests/issue51_sandbox_env_inheritance.rs` — arm 3 wrapped in a
`#[cfg(not(target_os = "windows"))] { … }` block, with the root cause recorded
in the comment (so it is not re-diagnosed as a temp-path bug). Arms 1–2 — the
actual issue-#51 invariant (sandbox transports receive ONLY the explicit
`env` slice) — keep running on Windows, so the file is not skipped wholesale.

One test is kept (not split into two): `.dev/AGENTS.md` requires env-var tests
to be ONE test (`set_var` is process-global, `cargo test` is parallel).

## Verification (macOS worktree)

- baseline `cargo test -p recursive-agent --test issue51_sandbox_env_inheritance`
  — 1 passed (pre-change, arm 3 active).
- post-change, same command — 1 passed.
- Windows-build simulation: flipped the gate to `#[cfg(target_os = "windows")]`
  (arm 3 compiled out, exactly what the Windows target sees), re-ran the same
  command — compiles clean (0 warnings: no unused-`tmp`/dead-code fallout) and
  1 passed; gate then reverted and `git diff` re-checked.
- `cargo fmt --all -- --check` — clean.
- `cargo clippy --test issue51_sandbox_env_inheritance --all-features -- -D warnings`
  — clean.
- No product code touched; no new deps.

Windows greenness itself is confirmed only by the next `windows-latest` run;
the arm's failure was a spawn-time "no such shell", which removing the arm
removes deterministically. `ci.yml`'s windows `continue-on-error` row is left
untouched (goal-410 territory).

## Left alone (deliberately, out of scope)

- `src/tools/transport_layer/transport.rs` still hard-codes `/bin/sh`, so the
  local (`none`) tier cannot execute on Windows at all — a product gap for a
  Windows-support issue, not this fixture fix.
- The stale anchor paths in the test file's header doc (`src/tools/shell.rs`,
  `src/tools/e2b_provider.rs` — both moved under `src/tools/execution/` /
  `src/tools/transport_layer/`) were left as-is to keep the diff surgical.
