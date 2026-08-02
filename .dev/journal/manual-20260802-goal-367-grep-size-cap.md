# Manual edit: goal-367 — cap file size in the Grep tool's read (OOM defense)

**Date**: 2026-08-02
**Goal**: Prevent OOM on plausible workspace contents (multi-GB logs / data dumps / bundles)
by adding a file-size gate before `read_to_string` in the Grep tool (`src/tools/search.rs`),
plus explicit `follow_links(false)` on the WalkDir builder. Agent kernel / run loop untouched;
grep search logic unchanged; no new deps.

## Files touched

- `src/tools/search.rs`
  - Added `const MAX_GREP_FILE_BYTES: u64 = 1024 * 1024;` near the top (after the other
    `DEFAULT_*` constants).
  - In `SearchFiles::execute`, added `.follow_links(false)` to the `WalkDir::new(&scope)`
    builder (was already the default; explicit now for documented symlink-loop safety).
  - Immediately before `std::fs::read_to_string(path)`: added
    `let Ok(meta) = std::fs::metadata(path) else { continue; };` and
    `if meta.len() > MAX_GREP_FILE_BYTES { continue; }` — silent skip, matching the
    existing binary-extension skip (`:165`-ish) which also just `continue`s.
  - Added test `grep_skips_files_larger_than_cap` in the `#[cfg(test)]` module.

## Design decisions

1. **Cap = 1 MiB constant, per the goal's recommendation.** Not configurable this goal.
   `meta.len()` is the byte length; `MAX_GREP_FILE_BYTES` is `u64` to compare directly.

2. **Silent skip (not surfaced).** The goal says to match the existing binary-extension
   skip style: that code silently `continue`s, so large files are skipped silently too.
   No new reporting mechanism. (The goal's constant doc-comment template mentioned "with a
   reason emitted in the result"; I adjusted the comment to say "silently, matching the
   binary-extension skip" so the comment is factually accurate.)

3. **`follow_links(false)` only — no `max_depth`.** The goal explicitly says grep should
   recurse fully; only the symlink explicitness is in scope.

4. **Metadata guard placement.** `metadata()` is called on the `entry.path()` after the
   extension heuristic, immediately before the read — this avoids an extra syscall for
   binary-extension files that are skipped anyway, and guarantees nothing between the
   check and the read can read the file (no TOCTOU concern for a read-only tool; worst
   case a file grows between metadata and read, which `read_to_string` handles normally).

## Tests added

- `tools::search::tests::grep_skips_files_larger_than_cap` — writes `big.log` of
  `MAX_GREP_FILE_BYTES + 1` bytes containing a matching "match" line (asserts the body
  really exceeds the cap before running), writes a small matching `small.txt`, asserts
  the small hit is returned and `big.log` is absent from results.

## Verification

- `cargo test -p recursive-agent --lib search::tests::grep_skips` → 1 passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` → clean.
- `cargo fmt --all` → clean.
- `cargo test --workspace` → pending at journal time (checked separately).
- `rg "read_to_string" src/tools/search.rs` → metadata guard immediately before it.
- `git diff --stat` → `src/tools/search.rs` (+32) only; no other files touched.
