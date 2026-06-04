# Goal 231: Static Invariant Checks in review-changes.sh

## Summary

Add static grep-based checks to `.dev/scripts/review-changes.sh` that
enforce high-risk AGENTS.md invariants directly in the diff, without relying
on the LLM review pass.

## Motivation

The current self-review pass is a single-turn LLM call. LLMs can miss subtle
invariant violations. Three invariants are cheap to verify mechanically and
expensive to miss:

1. **No `unwrap()`/`expect()` in non-test code** (Invariant #5)
2. **Sandbox not bypassed** — no direct `std::fs` or `std::path` ops outside
   `tools::resolve_within` (Invariant #3)
3. **No new `Error` variants that short-circuit transcript save** (Invariant #7)

## What to implement

In `.dev/scripts/review-changes.sh`, after the LLM review call and before
writing the final JSON verdict:

1. Parse the diff for lines added (`^+`) in `src/` files (excluding test
   modules — lines inside `#[cfg(test)]` blocks or `mod tests {}`).
2. Run the following grep checks on those added lines:
   - **unwrap_check**: match `\.unwrap()` or `\.expect(` — flag as issue if
     found outside a test context
   - **sandbox_check**: match `std::fs::` or `Path::new(` or `PathBuf::from(`
     without a preceding `resolve_within` in the same hunk — flag as
     potential sandbox bypass (warn, don't auto-reject; may be legitimate)
3. If any check fires, append an entry to the `issues` array in the review
   JSON with `severity: "critical"` for unwrap, `severity: "warning"` for
   sandbox.
4. If a `critical` issue is found, force `verdict: "request_changes"`
   regardless of what the LLM returned.

## Implementation notes

- Keep it simple: the checks run on the raw diff text via grep/awk in bash.
  No new dependencies.
- The sandbox check is intentionally a warning (not auto-reject) because
  `Path::new` appears legitimately in some tool impls that do call
  `resolve_within`. The LLM reviewer can then inspect the warning.
- Add a comment block at the top of the new section explaining what each
  check is for and which invariant it maps to.
- These checks apply to `.dev/scripts/review-changes.sh` which is in `.dev/`,
  not product code. This goal explicitly modifies `.dev/scripts/`.

## Acceptance

```bash
# Simulate a diff containing an unwrap() in src/tools/fs.rs (not in tests)
# Run review-changes.sh and confirm:
# 1. The output JSON contains an issue with severity "critical"
# 2. The verdict is "request_changes"
```

The check must not false-positive on legitimate `unwrap()` uses inside
`#[cfg(test)]` blocks.
