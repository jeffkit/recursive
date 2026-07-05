# Manual edit: p0-arch-real-bugs

**Date**: 2026-07-05
**Goal**: Land the two real P0 bugs called out in the architecture review
(orphan shell processes on timeout + the INSECURE_OK footgun) and the
P0-C docs-honesty sync that the previous p0-arch-fixes commit did not
cover (`.dev/AGENTS.md` layout, `Cargo.toml` description, README hero).

**Files touched**:
- `src/tools/shell.rs`        — RunShell timeout now kills the child
- `src/http/auth.rs`          — INSECURE_OK bypass gated on debug_assertions
- `.dev/AGENTS.md`            — layout section sync'd to the post-G219 reality
- `Cargo.toml`                — description no longer claims "minimal"
- `README.md`                 — hero text reflects the actual surface area

**Tests added**:
- `tools::shell::tests::timeout_kills_child_process` — pins that the
  child PID is gone within ~5 s of a timeout. Pre-fix this test hangs
  for the full 30 s sleep; post-fix it returns in <1 s. Uses `exec`
  in the sh script so the captured PID is the same PID `start_kill`
  targets — covering the case where sh runs the command in-process
  vs spawning a child would otherwise differ.
- `http::auth::tests::insecure_ok_bypass_is_gated_on_debug_assertions` —
  source-grep pin that the bypass is conditional on `cfg!(debug_assertions)`
  AND that there's an explicit release branch that surfaces the misuse
  warning. Mirrors the pattern in `src/http/mod.rs::goal_272_*`.

**Notes**:

- **Why both `kill_on_drop(true)` AND explicit `start_kill()`?** Defence
  in depth. `start_kill()` makes the intent visible at the call site
  and guarantees the OS reaps the child promptly; `kill_on_drop(true)`
  catches the case where a future refactor adds another early return
  (panic, `?` propagation, etc.) without remembering to kill the child.
  Either alone is fragile; together they cover both the "I forgot" and
  "I didn't know" failure modes.

- **Why source-grep for the INSECURE_OK gate instead of a runtime test?**
  `cfg!(debug_assertions)` is fixed at compile time and the test binary
  is always compiled with debug_assertions on, so a runtime test cannot
  observe release-build behaviour. Source-grep at least pins the
  structural invariant — if a future refactor removes the
  `cfg!(debug_assertions)` gate, this test breaks in CI. The trade-off
  is that semantic regressions inside the gate (e.g. negating it) are
  not caught, but the gate's presence is the load-bearing contract.

- **Why not bind `libc` for the orphan-process test?** AGENTS.md
  invariant #6: "no new deps without justification". Spawning `kill -0`
  as an external command is slower but zero-dependency and equally
  decisive — the test's only assertion is "is this PID gone". Adding
  `libc` as a dev-dependency for one signal-send would be unjustified.

- **The pre-existing INSECURE_OK env-var race in `handlers.rs:1929`
  is not addressed by this change.** It is a pre-existing flake:
  parallel tests in `handlers.rs` set INSECURE_OK=1 for fixture
  convenience, and the auth test that asserts 503-without-INSECURE_OK
  occasionally sees the residue. CLAUDE.md / `.dev/AGENTS.md` already
  document the "env-var tests must be ONE test, not many" rule, but
  fixing the existing violations is a separate cleanup. This PR makes
  the race marginally more likely to fire (one extra env::var read in
  the middleware path) but the underlying bug is pre-existing.

- **AGENTS.md layout update lists every module that exists today.**
  This is intentionally exhaustive — future "kernel vs platform"
  restructuring (recommended in the architecture review as recommendation
  #1) will trim this list back down, at which point the layout section
  should be updated again to match.

**Verification**:
- `cargo test --workspace` — green (1068 lib + integration tests,
  plus 661 in recursive-tui, plus TUI harness tests)
- `cargo clippy --all-targets --all-features -- -D warnings` — green
- `cargo fmt --all --check` — green
- `gitnexus_impact RunShell upstream` — MEDIUM risk, 1 direct caller
  (TUI App::submit_prompt); change is internal to the timeout branch
  and does not alter the return type or error variant.
- `gitnexus_impact auth_middleware upstream` — LOW risk, 0 callers
  (helper function).
