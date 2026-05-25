# Goal 31 — `apply_patch` supports `*** Delete File:`

## Why

V4A's full grammar includes `*** Add File:`, `*** Update File:`, and
`*** Delete File:`. Our `apply_patch` implements the first two; if
the agent ever needs to remove a file (orphan modules, dead scripts,
deprecated tests), it must fall back to `run_shell rm`, which:

- mutates the working tree without going through the patch audit
  trail,
- skips the workspace-sandbox check (path validation),
- breaks the rule "agent never edits via shell" (AGENTS.md hard
  limit, though `rm` is technically a "delete" not an "edit").

Add proper `*** Delete File:` support to close the gap.

## Scope

Touches: `src/tools/apply_patch.rs` only (parsing + tests in same
file).

1. Extend the parser to recognize a third operation:
   ```
   *** Begin Patch
   *** Delete File: <path>
   *** End Patch
   ```
   - The block has **no hunks** — just the header and the End Patch
     marker. Parser should accept zero hunks for `Delete File:` and
     return an error if any `@@` / `+` / `-` lines follow.

2. Execution:
   - Resolve `<path>` via the existing `resolve_within` sandbox.
   - If the file does not exist → `Error::Tool` with message
     `delete file: <path>: file does not exist`. Do NOT treat
     missing files as no-ops; that would mask agent mistakes.
   - If the path is a directory → `Error::Tool` with message
     `delete file: <path>: is a directory, not a file`.
   - Otherwise `tokio::fs::remove_file(&abs)` and return a success
     string `"deleted: <path>"`.

3. Composite patches:
   - A single `*** Begin Patch ... *** End Patch` block can mix
     Update / Add / Delete operations. The existing parser likely
     already separates blocks; just thread the Delete variant
     through the existing iteration order.

4. Tests in the same file:
   - **Test A**: a patch with only `*** Delete File: foo.txt` (after
     creating `foo.txt`) returns `deleted: foo.txt` and `foo.txt`
     no longer exists on disk.
   - **Test B**: deleting a non-existent file returns the
     `file does not exist` error.
   - **Test C**: deleting a directory returns the
     `is a directory` error.
   - **Test D**: a mixed patch — Update one file + Delete another
     — applies both atomically (or both fail; one-shot semantics).
   - **Test E** (optional): a patch with `*** Delete File:` followed
     by stray hunk lines errors with a parser message indicating
     "Delete File takes no hunks".

## Acceptance

- `cargo build` green.
- `cargo test` green (138 baseline + 4 new minimum = 142+).
- `cargo clippy --all-targets -- -D warnings` green.
- `cargo fmt --all` clean.
- The system prompt in `src/config.rs::default_system_prompt` is
  NOT changed in this goal (its V4A worked example covers Update;
  adding a Delete example is a separate follow-up).

## Notes for the agent

- This is **scoped to one file** — `src/tools/apply_patch.rs`. Don't
  touch `tools/mod.rs`, `main.rs`, `config.rs`.
- Use `apply_patch` itself to make the edits (dogfood). The dry-run
  mode from goal-25 might help you validate a hunk before
  committing.
- The parser likely uses an enum `Operation::Update { ... }` or
  similar. Add `Operation::Delete { path: PathBuf }` and thread
  through the existing match arms.
- This goal is **borderline hard** — if you BudgetExceed (100 steps),
  the auto-resume in self-improve.sh will give you another 100 steps.
  Don't worry about that; just stop calling tools when you've made
  meaningful progress and let the wrapper do its thing.
- `.to_string()` over `.into()` in tests. If env-var tests are
  involved (unlikely here), one consolidated test only — see
  AGENTS.md section 5.
