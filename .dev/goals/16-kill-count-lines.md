# Goal 16 — Remove the obsolete `CountLines` tool

## Why

`src/tools/count_lines.rs` predates `run_shell`. Every functionality it
provides (`wc -l <path>`) is now expressible by the agent itself via
`run_shell: wc -l path/to/file`. It is dead weight in the tool list —
each tool spec sent to the LLM costs prompt tokens on every step, so
keeping a redundant tool around taxes every run.

This goal removes the tool entirely. It is a cleanup, not a feature.

## Scope

1. Delete the file `src/tools/count_lines.rs`.
2. In `src/tools/mod.rs`, remove the `pub mod count_lines;` and any
   re-export of `CountLines`.
3. In `src/main.rs` (function `build_tools`), remove the registration
   line for `CountLines`.
4. Remove any tests in `src/tools/count_lines.rs` (deleted with the file)
   and any references to it elsewhere (rg for `CountLines` and
   `count_lines` — both `src/` and `tests/`).

## Acceptance

- `cargo build` succeeds.
- `cargo test` succeeds (the existing 113 tests stay green).
- `cargo clippy --all-targets -- -D warnings` succeeds.
- `recursive tools` no longer lists a `count_lines` tool. The simplest
  smoke check is `cargo run --quiet -- tools 2>/dev/null | grep -c
  count_lines` should print `0`.

## Notes for the agent

- Use `apply_patch` for the edits to `src/tools/mod.rs` and
  `src/main.rs`. They are small targeted removals — single hunks each.
- For deleting the file, the `apply_patch` format is
  `*** Delete File: src/tools/count_lines.rs`. (No content lines
  needed.)
- This goal should be a 5–10 step run. If you find yourself past
  step 20, stop and write what's blocking — something has gone wrong.
