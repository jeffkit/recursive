# Goal 29 — `search_files` case-insensitive flag

## Why

`search_files` already supports an optional `regex: bool` flag
(goal-13). For literal-string searches (regex=false) it's
case-sensitive, and there's no easy way to find e.g. "TODO" /
"todo" / "Todo" in one pass without rewriting as a regex with
`(?i)`. Add a `case_insensitive: bool` flag that works in both
literal and regex modes.

## Scope

Touches: `src/tools/search.rs` only (plus tests in the same file).

1. Extend the tool's JSON schema in `spec()`:
   - Add an optional `case_insensitive: boolean` parameter (default
     `false`). Document it: "If true, matching ignores ASCII case.
     Works in both literal and regex modes; in regex mode this is
     equivalent to wrapping the pattern in `(?i:...)`."

2. In `execute()`:
   - Parse `case_insensitive` from args (default false).
   - Literal mode: use `haystack.to_ascii_lowercase().contains(
     &needle.to_ascii_lowercase())` when the flag is set, otherwise
     existing `contains`.
   - Regex mode: when the flag is set, build the regex with
     `regex::RegexBuilder::new(pattern).case_insensitive(true).build()?`
     instead of `Regex::new`.

3. Tests in the same file:
   - **Test A**: literal mode, case_insensitive=true, finds "TODO"
     in a file containing "todo".
   - **Test B**: regex mode, case_insensitive=true, pattern
     "FOO\\d+" matches "foo123" in the file.
   - **Test C** (regression): case_insensitive=false (or omitted)
     preserves existing case-sensitive behavior.

## Acceptance

- `cargo build` green.
- `cargo test` green (132 baseline + 3 new = 135).
- `cargo clippy --all-targets -- -D warnings` green.
- `cargo fmt --all` clean.

## Notes for the agent

- This is **scoped to one file** — `src/tools/search.rs`. Don't
  touch `src/tools/mod.rs` or anything else.
- Read the existing `regex` flag handling first — that's exactly
  the shape you want.
- Use `apply_patch`. `.to_string()` over `.into()` in tests.
- The `regex` crate is already a dependency; no Cargo.toml change.
