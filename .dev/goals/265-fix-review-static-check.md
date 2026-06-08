# Goal 265 — Fix review-changes.sh static check for test bodies

## Summary

The static review script `.dev/scripts/review-changes.sh` counts
`unwrap()`/`expect()` and raw `fs`/`PathBuf` operations across all
changed Rust source lines. The grep-based filter only excludes lines
containing the `#[cfg(test)]` marker itself — it does NOT skip the
entire body of `#[cfg(test)] mod tests { ... }` blocks.

This caused 74 false-positive "production unwrap" warnings during goal
264 landing, triggering two unnecessary revision rounds and ultimately
requiring the "commit with warnings" fallback.

## What to fix

In `.dev/scripts/review-changes.sh`, replace the current line-level
grep filter with a state-machine approach that:

1. Detects entry into `#[cfg(test)] mod ... {` scope (tracking brace
   depth to find the matching close).
2. Skips all lines inside that scope when counting unwrap/expect/fs
   violations.
3. Also skip `#[test]` annotated function bodies (single-level brace
   tracking is fine for this).

A Python snippet or awk state machine is acceptable. The script is
bash-heavy but already calls `python3` for other checks.

## Acceptance criteria

- `cargo test --lib` and `cargo test --bin recursive` still pass after
  the script change.
- Running the check against the 264 diff no longer reports test-body
  unwraps as production violations.
- The one real production unwrap that 264 found (and fixed) would still
  be caught.
- Script is self-contained; no new dependencies.

## Complexity: easy

## Out of scope

- Do not touch any Rust source files.
- Do not change review policy thresholds or other checks in the script.
