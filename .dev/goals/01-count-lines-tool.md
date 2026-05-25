# Goal: add a `count_lines` tool

Add a new tool called `count_lines` that returns the number of lines in a
text file inside the workspace.

## Requirements

1. Create `src/tools/count_lines.rs` with a `CountLines` struct that holds
   the workspace root (like the existing `ReadFile`).
2. Implement the `Tool` trait for it:
   - `spec()` returns name `"count_lines"`, a 1-sentence description, and a
     JSON-schema with one required string property `path`.
   - `execute(args)` resolves the path via `super::resolve_within`, reads the
     file, returns the number of `\n`-separated lines as a string.
3. Re-export `CountLines` from `src/tools/mod.rs` (next to the existing
   re-exports) and add `pub mod count_lines;`.
4. Register `CountLines` in `src/main.rs`'s `build_tools()` alongside the
   others.
5. Add at least two unit tests in `count_lines.rs`:
   - happy path: writes a file with N lines, asserts the tool returns N.
   - sandbox: asserts a path with `..` is rejected.
6. Run `cargo build` and `cargo test` and confirm both are green.

## Definition of done

- `cargo test` passes, including the new tests.
- `./target/debug/recursive tools` JSON output includes `count_lines`.
- No other files modified beyond what is listed above.

Write a final summary listing the files you created/modified and the test
result.
