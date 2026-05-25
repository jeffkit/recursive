# Goal 22 — `apply_patch` better error when context is non-unique

## Why

When V4A context matches multiple locations in the target file, the
agent gets a terse error and tends to retry the *same* patch hoping
something changes — burning steps and triggering the anti-stuck
heuristic. This is exactly what killed goal-17 on its second
attempt.

The current error reads something like:

  `update src/foo.rs: hunk 2 pattern matches 2 locations; add an
   @@ anchor line above the hunk to disambiguate`

That's *informative* but doesn't tell the agent WHAT to put in the
`@@ anchor` line. Worse, the agent sometimes interprets the message
as "the patch is wrong" and tries to rewrite the lines, not realising
it just needs to pick a unique-anchor line.

## Scope

Touches: `src/tools/apply_patch.rs` only.

1. When the patch engine detects a hunk whose context matches >1
   location, change the error message to include up to **3 specific
   line examples** that would each disambiguate, drawn from lines
   surrounding *one of* the matched locations. Example desired
   shape:

   ```
   update src/foo.rs: hunk 2 context matches 3 locations.
   Add an `@@ <anchor>` line above the hunk with one of these
   unique nearby lines:
     @@ fn handle_request(req: Request) -> Response {
     @@     pub const VERSION: &str = "1.0";
     @@     "Endpoint registered"
   ```

   Pick the lines pragmatically: walk 5 lines before and 5 lines
   after the first matched location; for each, check that the
   exact line text appears only once in the whole file; keep the
   first 3 that pass the uniqueness check.

2. **Tests**: add at least one test in
   `src/tools/apply_patch.rs::tests` that:
   - Constructs a file with a duplicated context block and an
     `apply_patch` call whose context matches both.
   - Asserts the error message contains both `"matches"` (or the
     equivalent indicator of multiple matches) and at least one
     `@@ ` suggestion that actually appears in the file as a
     unique line.

## Acceptance

- `cargo build` green.
- `cargo test` green (119 baseline + ≥1 new = 120 minimum).
- `cargo clippy --all-targets -- -D warnings` green.
- `cargo fmt --all` clean.

## Notes for the agent

- Read `src/tools/apply_patch.rs` end-to-end first — the patch
  engine is non-trivial and you need to know where the
  "ambiguous context" error originates.
- The improved error message is **additive only** — do not change
  the success path or the error path for other failure modes
  (file not found, hunk doesn't apply, etc.).
- Be careful not to scan the whole file for every candidate line —
  doing so is `O(n²)` and pointless for our needs. A simple
  `lines.iter().filter(|&l| count_occurrences(l) == 1)` over a
  ±5-line window is fine.
- Use `apply_patch` for the implementation. Single-file change;
  should fit in 8–12 steps.
- **In tests, prefer `.to_string()` over `.into()` for string
  literals.**
