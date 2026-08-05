# Goal 389 — TUI prompt history persists across sessions

**Roadmap**: TUI usability (from `docs/tui-fake-cc-gap.md` §2 — history
persistence still 🔴). In-session ↑/↓ and Ctrl+R work; quitting the TUI loses
the prompt history.

**Design principle check**:
- Implemented as: a bounded JSONL history file under the existing Recursive
  home convention, loaded into the current TUI input state and persisted on
  successful submit. Pure TUI + paths; no kernel changes.
- ❌ Does NOT branch `run_core.rs::run_inner` or change transcript/session
  JSONL formats.
- Follow `.dev/goals/_TEMPLATE-tui-acceptance.md`; tests must cover the App
  wiring as well as pure file helpers.

## Why (verified 2026-08-04)

1. `docs/tui-fake-cc-gap.md` marks cross-session prompt history as 🔴.
2. `crates/recursive-tui/src/input_state.rs` has `load_history` for in-memory
   index navigation only; there is no prompt-history file I/O.
3. Session transcripts persist, but they are not a safe replacement for the
   small command/prompt history used by ↑/↓ and Ctrl+R.

## Scope (do exactly this, no more)

### 1. Fixed storage contract

- Add a path helper that follows `src/paths.rs`'s existing Recursive-home
  convention. Store one global TUI prompt-history file under that home; do not
  invent another HOME-style environment variable for tests.
- Use JSONL: one JSON string per physical line, so UTF-8 and embedded newlines
  round-trip without an ambiguous raw-line format.
- Keep the newest **1000** entries; when the cap is exceeded, discard the
  oldest entries deterministically.
- On Unix, create/replace the file with mode `0600`.
- Empty prompts and existing quit-only inputs must not be stored. Preserve the
  current in-memory rules for mode-prefixed prompts (`!`, `#`, `/`).
- Malformed/truncated JSONL records are skipped with a debug/warn diagnostic;
  one bad line must not prevent the TUI from starting or loading valid lines.

### 2. Wire load and persistence

- Load persisted entries into the same in-memory structure used by ↑/↓ and
  Ctrl+R during App/input initialization.
- After a prompt is accepted successfully, persist the updated bounded
  history before the value can be lost. A final best-effort flush on clean
  exit is allowed, but exit-only persistence is not sufficient.
- Use the repository's atomic-write primitive or an equivalent temp-file +
  rename strategy. Concurrent TUI instances must never produce torn/invalid
  JSONL. If the existing primitives cannot merge concurrent writers, define
  and document deterministic last-writer-wins semantics and test file
  integrity; do not claim cross-process merge support.
- I/O failure must not crash or block the interactive session. Surface a
  diagnostic through the existing logging/event path.

### 3. Tests (same change — TUI gates)

Add deterministic temp-directory tests that take an explicit path; do not set
a process-global history-path environment variable in parallel tests.

Required cases:

1. JSONL round-trip for ordinary, UTF-8, and multi-line prompts;
2. `!`, `#`, and `/` prefixes survive save/load with the same navigation
   semantics as current `strip_history_prefix` handling;
3. 1001+ entries retain exactly the newest 1000 in order;
4. malformed/truncated records are skipped while later valid records load;
5. atomic/concurrent-write test proves the file remains valid JSONL under the
   documented last-writer/locking semantics;
6. restart test: submit through `App`/`Harness`, drop the App, construct a new
   App from the same temp history path, and retrieve the prior prompt through
   the existing history navigation path.

Pure `load_prompt_history` / `save_prompt_history` tests alone are not enough:
a mutation that removes the submit-time save call must be caught by the
App/Harness regression test.

## Files NOT to touch

- Kernel / HTTP session store beyond reusing a small path or atomic-write
  helper.
- Transcript JSONL format or session replay semantics.
- `.dev/flows/`, `.flowcast/`, or TUI gate policy.
- REPL history sharing; this goal is TUI-only unless a pre-existing shared
  prompt-history abstraction already exists.

## Acceptance

- `cargo test -p recursive-tui` green.
- The six named history tests above exist and pass by name.
- `cargo clippy -p recursive-tui --all-targets -- -D warnings` clean;
  `cargo fmt --all -- --check` clean.
- `.dev/scripts/tui-test-presence.sh` exit 0.
- `.dev/scripts/tui-mutants.sh` runs against every touched
  `crates/recursive-tui/src/` file; no surviving mutant can remove load,
  submit-time save, cap enforcement, or malformed-record handling.
- Unix-specific test confirms newly created history file is not group/world
  readable.
- Journal: `.dev/journal/manual-20260804-goal389-prompt-history.md` records the
  path, JSONL schema, concurrency semantics, exact tests, and mutants result.

## Notes for the agent (traps)

- Do not parse raw lines as prompts: multi-line input needs JSON escaping.
- Do not mutate process-global environment in tests; Cargo runs tests in
  parallel.
- Do not hold a UI/runtime lock across filesystem I/O. Snapshot the bounded
  entries, then persist the snapshot.
- Atomic rename prevents torn files but does not automatically merge two
  writers. State the supported multi-process semantics honestly.
- Keep mode prefixes lossless; `strip_history_prefix` is a navigation/display
  concern, not permission to erase the stored prefix.
