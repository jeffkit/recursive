# Goal 26 — `read_file` line range support

## Why

`read_file` currently returns the **entire** file. When an agent needs
to inspect a 500-line module to add a single hunk near the bottom, it
pulls all 500 lines into the transcript — which then has to round-trip
through every subsequent LLM call. On `src/tools/apply_patch.rs`
(~580 lines) this is observably expensive: goal-22's run cost
**$0.4186** in part because of repeated full re-reads.

Add an optional `start_line` / `end_line` pair to `read_file`. When
both are set, return only that slice, prefixed with a small header
indicating the visible range and total line count. Both bounds are
1-indexed and inclusive (matches the existing `LINE_NUMBER|CONTENT`
display convention).

## Scope

Touches: `src/tools/fs.rs` only (plus tests in the same file).

1. Update `ReadFile::spec()`:
   - Add two optional integer parameters: `start_line` (>= 1) and
     `end_line` (>= start_line). Document them in the schema:
     "Optional 1-indexed inclusive line range. If both are provided,
     only that slice is returned. If only one is provided, treat the
     other as the file boundary."

2. Update `ReadFile::execute()`:
   - Parse `start_line` and `end_line` from args (both optional `u64`).
   - After the UTF-8 decode, count total lines (`content.lines().count()`).
   - If no range: existing behavior unchanged (return the whole file).
   - If range: clamp to `[1, total_lines]`, validate
     `start_line <= end_line`, and return the slice formatted as:

     ```
     # range: lines <s>-<e> of <total>
     <slice content>
     ```

     where `<slice content>` is `content.lines().skip(s-1).take(e-s+1).join("\n")`.

3. Errors:
   - `start_line == 0` or `end_line == 0` → `Error::BadToolArgs`
     (1-indexed only).
   - `start_line > end_line` → `Error::BadToolArgs`.
   - `start_line > total_lines` → `Error::BadToolArgs` with a message
     like "start_line N exceeds total lines M".

4. Tests in the same file:
   - **Test A**: range `start_line=2, end_line=3` on a 5-line file
     returns the expected slice with the `# range:` header.
   - **Test B**: omitting both fields returns the original full file
     (regression-prevention for existing callers).
   - **Test C**: `start_line=10, end_line=5` returns `BadToolArgs`.

## Acceptance

- `cargo build` green.
- `cargo test` green (123 baseline + 3 new = 126).
- `cargo clippy --all-targets -- -D warnings` green.
- `cargo fmt --all` clean.

## Notes for the agent

- This is **scoped to one file** — `src/tools/fs.rs`. Do NOT touch
  `src/tools/mod.rs`, `src/main.rs`, `src/config.rs`, or
  `src/tools/apply_patch.rs` (apply_patch is being changed in a
  concurrent branch).
- Use `apply_patch` for source edits.
- Prefer `.to_string()` over `.into()` for string literals in tests
  (AGENTS.md section 5).
- The `max_bytes` cap on the underlying file still applies — a huge
  file is rejected before line slicing even happens. That's
  intentional; don't change `max_bytes` handling.
- If the V4A anchor uniqueness bites you on `ReadFile`'s `execute()`
  (the function is small), anchor on the surrounding `impl Tool for
  ReadFile` block instead of inside the function body.
