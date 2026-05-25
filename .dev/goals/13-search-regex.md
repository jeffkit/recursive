# Goal 13 — Regex mode for `search_files`

## What

Extend the `search_files` tool (added in goal 11) with an optional
`regex: bool` flag. When `true`, treat `pattern` as a regular
expression compiled with the `regex` crate. When `false` or absent,
keep today's literal substring semantics.

Invalid regex must surface as a clean `Error::BadToolArgs`, not a
panic and not a generic ERROR result.

## Why

Substring search covered the common case — finding identifiers or
literal strings. But there's a real gap for agent-driven searches:

- Finding all `fn foo_*` definitions.
- Excluding test code with `^[^/]*(?<!_test)\.rs$` patterns.
- Looking for "the line that mentions either X or Y".

The agent currently has to fall back to `run_shell` with `rg`/`grep`
for these, which defeats the whole reason we added `search_files`
in goal 11.

Adding regex is one new arg, one new branch, and roughly thirty
lines of code including tests. It's a high-leverage extension.

## Scope (do exactly this, no more)

### 1. `Cargo.toml`

Add to `[dependencies]`:

```toml
regex = "1"
```

Keep the version unconstrained beyond the major; `regex = "1"` is
stable and widely cached.

### 2. `src/tools/search.rs`

In the `SearchFiles::execute` method, parse `regex` from args:

```rust
let use_regex = args.get("regex").and_then(|v| v.as_bool()).unwrap_or(false);
```

If `use_regex` is `true`, compile the pattern once and use it for
matching:

```rust
use regex::Regex;

let re_opt: Option<Regex> = if use_regex {
    Some(Regex::new(pattern).map_err(|e| Error::BadToolArgs {
        name: "search_files".into(),
        message: format!("invalid regex: {e}"),
    })?)
} else {
    None
};
```

In the existing inner loop where we test `line.contains(pattern)`:

```rust
let is_match = match &re_opt {
    Some(re) => re.is_match(line),
    None => line.contains(pattern),
};
if is_match { /* existing push */ }
```

Update the `spec()` schema to advertise the new arg:

```jsonc
"regex": {
  "type": "boolean",
  "description": "If true, interpret `pattern` as a regular expression (Rust regex crate syntax). Default false (literal substring)."
}
```

Update the `description` to mention the new mode briefly.

### 3. Tests

Add to the `#[cfg(test)] mod tests` block in `src/tools/search.rs`:

1. `regex_mode_matches_pattern` — write file with `fn foo() {}` and
   `fn bar() {}`, search with `pattern="fn f\\w+"` + `regex=true`,
   assert only `foo` matches.
2. `regex_mode_invalid_pattern_is_bad_args` — `pattern="(unclosed"` +
   `regex=true`, assert `Err(Error::BadToolArgs { .. })` mentioning
   "invalid regex".
3. `literal_mode_treats_pattern_as_substring` — `pattern="a.c"`
   without `regex`, search in `"abc\nadc"`, assert *zero* matches
   (literal `a.c` is not present; in regex mode it would match
   both lines). Confirms backward compat.
4. `regex_mode_with_path_scope` — combine `regex=true` with the
   existing `path=` scope from goal 11, assert it still scopes.

## Out of scope

- Multi-line / dot-matches-newline regex flags. Default flags only.
- Case-insensitive flag (`(?i)` in the pattern works; we don't need
  a separate arg).
- Replacing the substring code path. Both modes coexist.
- Streaming results / async iteration over matches.

## Definition of done

- `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test` all green.
- 4 new tests pass; existing `search_files` tests untouched.
- A goal-13 invocation with `regex=true` and a broken pattern returns
  `BadToolArgs`, not a panic.
- `regex` is the only new dependency added.

## Notes for the agent

- The change in `execute()` is small: a couple of helper bindings
  plus a one-line match-or-contains. Use `apply_patch`.
- Cargo.toml is small; `apply_patch` works fine if the `[dependencies]`
  block is unambiguous in your read. Otherwise `write_file` the
  whole `Cargo.toml`.
- Don't change the existing schema entries for `pattern`, `path`,
  `max_results`. Adding `regex` to the same object is one extra
  property on `properties`, not a replacement.
