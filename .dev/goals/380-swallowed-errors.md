# Goal 380 — Propagate swallowed errors: checkpoint git-add, save_open_interrupts, hook stdin writes

**Roadmap**: Kernel / reliability — three P1 error-swallowing sites silently lose data or
make decisions on truncated input

**Design principle check**:
- Implemented as: three localized error-propagation fixes (return `Result`/check
  `status`/log-and-fail) in `src/checkpoint.rs`, `src/http/handlers.rs`,
  `src/hooks/external.rs`. No new deps. No kernel/run-loop structure changes.
- ❌ Does NOT touch the agent kernel invariants.

## Why (all three verified 2026-08-03 by reading the code)

1. **`src/checkpoint.rs:571` — `git add` failure silently corrupts diffs.**
   `write_workspace_tree()` runs `git add -A --force` via `git_cmd()` and discards the
   result with `let _ =`. The subsequent `git write-tree` (at `:606`-ish) uses the same
   temp index; if `add` failed (perms, lock, repo issue), the tree is stale/empty and the
   checkpoint's diff-vs-workspace silently misses real changes — checkpoints lose work
   without any error surfacing.

2. **`src/http/handlers.rs:1988` — `save_open_interrupts` swallows the persist write.**
   The function's doc comment says "Written atomically before emitting RunFinished with
   Interrupt outcome" — a crash-safety contract: pending tool-permission interrupts must
   survive across a crash/resume. But `crate::atomic::atomic_write(&path,
   json.as_bytes()).ok()` drops the `Err`; on disk-full/permission failure the pending
   interrupt is silently lost and a later resume won't re-prompt the user.

3. **`src/hooks/external.rs:782,835` — hook stdin writes swallowed.**
   Both `run_simple_hook` and `run_command_hook` do `let _ = stdin.write_all(...)` +
   `let _ = stdin.shutdown().await`. If the write fails (pipe broken, child exited
   early), the hook process may read a truncated/empty payload and still emit an
   allow/deny decision — and the tool trusts that decision. A deny-based hook can be
   bypassed by a broken pipe (hook sees empty input, denies nothing → allows), or a
   security hook's decision is made on garbage.

## Scope (do exactly this, no more)

### 1. `src/checkpoint.rs` — propagate `git add` failure

In `write_workspace_tree()`: capture the `git_cmd()...add` output and check
`.output().status` (mirror the existing write-tree handling pattern in the same file).
On non-zero status, return `Err` with a message including the git stderr (like the
surrounding error style, e.g. `Error::Tool { message: format!("git add failed: {}",
String::from_utf8_lossy(&output.stderr)) }`). Do not change the diff/tree logic
otherwise.

### 2. `src/http/handlers.rs` — make `save_open_interrupts` fail loudly

Change `save_open_interrupts` to return `Result<()>`; propagate (or at minimum
`tracing::error!`) when `create_dir_all` or `atomic_write` fails. Callers at `:1828` and
`:1986` handle the `Result` — if the surrounding flow can't fail the request, log an
error with the session dir + reason; never `.ok()` silently. Also fix
`clear_open_interrupts` (`:1997`) to log on `remove_file` failure (a stale
`.interrupts.json` re-surfaces as a phantom pending interrupt later).

### 3. `src/hooks/external.rs` — surface stdin write failures

In both hook runners: replace `let _ = stdin.write_all(...)` / `let _ =
stdin.shutdown()` with error handling that feeds the failure into the hook result
(use the existing `fail_mode` / `HookResult::from_fail_mode` machinery so an
unreadable-payload hook is treated per the configured fail mode — deny-closed
configs must NOT silently allow). Do not change hook timeouts or spawn handling.

### 4. Tests

- `checkpoint.rs`: a unit test where the git-add step fails (e.g. point the shadow dir /
  workspace at an invalid path so `git add` exits non-zero) → `write_workspace_tree`
  returns `Err` (skip if the existing harness makes this impractical — then at least a
  comment-pinned assertion that the status is checked; but prefer a real test).
- `handlers.rs`: `save_open_interrupts` returns an error when the target path is
  unwritable (e.g. session dir under a read-only parent) — assert the `Result` is `Err`.
- `hooks/external.rs`: a hook command that exits immediately on spawn (so stdin write
  fails) → the result is routed through `fail_mode` (deny-mode yields a deny/error
  result, not a silent allow). Mirror the file's existing test style.

## Files NOT to touch

- `src/run_core.rs`, `src/kernel.rs`, `src/runtime.rs` — kernel/run-loop invariants.
- `src/tools/*` — unrelated tools.
- `.dev/flows/`, `.dev/scripts/`, `.flowcast/`.

## Acceptance

- `cargo build --workspace` green.
- `cargo test --workspace` green.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.
- Grep: `rg "git add" src/checkpoint.rs` — the result is no longer `let _ =` discarded
  (status checked or error propagated).
- Grep: `rg "save_open_interrupts" src/http/handlers.rs` — signature is `-> Result<()>`
  (or errors are propagated at both call sites).
- Grep: `rg "write_all" src/hooks/external.rs` — no `let _ = stdin.write_all` remains.
- Headline tests by name: `cargo test --manifest-path Cargo.toml write_workspace_tree`,
  `cargo test --manifest-path Cargo.toml save_open_interrupts`,
  `cargo test --manifest-path Cargo.toml hook` — new tests green.

## Notes for the agent (traps)

- **Checkpoint's `let _ =` on the remove_file at the top (`:566`) is intentional-ish**
  (best-effort cleanup) — you may leave it, but the `git add` Result must be checked.
- **`save_open_interrupts` call sites** (`:1828`, `:1986`): read the surrounding
  function — if it returns `Result`, propagate; if it's an HTTP handler with an
  infallible signature, `tracing::error!` + continue is acceptable, but the write must
  never be silently dropped while the handler reports success (the comment contract).
- **Hook stdin failure → respect `fail_mode`.** The whole point is a deny-hook must not
  become allow-by-broken-pipe. `HookFailMode::Deny` should yield a deny result on
  input-write failure, exactly as it does on spawn failure (`:812`-ish shows the
  pattern).
- **cargo-fmt + clippy are enforced gates** — run both before finishing.
- **Journal**: write `.dev/journal/manual-<date>-goal380-swallowed-errors.md`.
