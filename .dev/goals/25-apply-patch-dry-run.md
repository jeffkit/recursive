# Goal 25 — `apply_patch` dry-run mode

## Why

Agents currently learn whether a V4A patch parses, finds its anchors,
and applies cleanly only by *applying it for real*. When a patch fails
in step N, the agent has often already destabilized the workspace with
prior changes, and the V4A error messages — even after goal-22 — are
easier to act on if the agent can ask "would this apply?" without
committing to it.

Add a `dry_run: bool` argument to the `apply_patch` tool. When true,
parse + locate + simulate the patch but write nothing to disk. Return
a structured success message describing what *would* change.

## Scope

Touches: `src/tools/apply_patch.rs` only.

1. Extend the tool's JSON schema (`spec()` method) with an optional
   `dry_run: boolean` parameter, defaulting to `false`. Document it
   in the description: "If true, validate and resolve the patch but
   do not write any files. Returns the same success/error message
   it would produce in a normal run."

2. In the `execute()` method:
   - Parse `dry_run` from arguments (default `false`).
   - Run through the existing parse/locate/apply pipeline as far as
     **building the post-patch content for each file**.
   - If `dry_run == true`: skip the actual `fs::write(...)` call(s)
     but still return a success string of the form:
     `"dry-run: would apply N hunk(s) across M file(s): <comma-separated paths>"`.
   - If `dry_run == false`: existing behavior unchanged.

3. Behavior on errors:
   - If the patch fails to parse or any hunk fails to resolve, return
     the **same error** as a non-dry-run would — regardless of the
     `dry_run` flag. The point is for the agent to discover failures
     cheaply.

4. Tests in the same file:
   - One test: dry-run on a known-good patch returns the
     `dry-run: would apply ...` string AND does NOT modify the
     target file (assert file content unchanged after).
   - One test: dry-run on a patch with an ambiguous anchor returns
     the same error as non-dry-run (reuse / adapt an existing
     ambiguity test if one exists).
   - Optional: one test confirming `dry_run: false` (the default)
     behaves identically to omitting the field.

## Acceptance

- `cargo build` green.
- `cargo test` green (123 baseline + 2 new = 125, +1 if you add the
  optional test = 126).
- `cargo clippy --all-targets -- -D warnings` green.
- `cargo fmt --all` clean.

## Notes for the agent

- This is **scoped to one file** by design. Do not touch
  `src/tools/mod.rs`, `src/main.rs`, or anything outside
  `src/tools/apply_patch.rs`.
- The cleanest implementation is usually: factor the existing
  apply logic into a helper that returns the *would-be-written*
  string per path, then conditionally call `fs::write`. If the code
  is already shaped this way, even better.
- Use `apply_patch` for all source edits (dogfood).
- `.to_string()` not `.into()` for string literals in tests
  (AGENTS.md section 5).
- If V4A context-uniqueness bites you on the test additions, anchor
  on a longer span — V4A wants 3+ unique context lines around each
  edit.
