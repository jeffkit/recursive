# Goal 27 — `run_shell` env-vars passthrough

## Why

The `run_shell` tool inherits the parent process env, but agents
sometimes need to **add or override** a single variable for one
command (e.g. `CARGO_TERM_COLOR=never`, `RUST_LOG=debug`,
`SOURCE_DATE_EPOCH=...`) without polluting their own process env.

Currently the only workaround is `run_shell({"command": "FOO=bar
cargo build"})`, which works in `bash` but the actual implementation
spawns via `sh -c` — fine in practice, but inline env-prefixing is
fragile under quoting. A proper structured field is cleaner.

## Scope

Touches: `src/tools/shell.rs` only (plus tests in the same file).

1. Extend the `run_shell` JSON schema:
   - Add an optional `env: object` parameter — a flat map of
     `string -> string`. Document it: "Optional extra env vars set
     for this command only. Values must be strings; non-string values
     are rejected. These add to (or override) the inherited env."

2. In `execute()`:
   - Parse the `env` field if present (`args.get("env").and_then(|v|
     v.as_object())`).
   - For each key/value pair: validate value is a string
     (`value.as_str()`), error with `Error::BadToolArgs` if not.
   - Apply via `Command::env(key, value)` on the existing
     `tokio::process::Command` builder. **Do NOT** call `Command::envs`
     with the whole map at once — iterate so the bad-value error
     points at the offending key.

3. Tests in the same file:
   - **Test A**: `run_shell` with `{"command": "echo $RECURSIVE_TEST_VAR",
     "env": {"RECURSIVE_TEST_VAR": "hello"}}` returns stdout containing
     "hello".
   - **Test B**: `env` with a non-string value (e.g. number) returns
     `BadToolArgs` mentioning the offending key name.
   - **Test C** (regression): omitting `env` works exactly as before.

## Acceptance

- `cargo build` green.
- `cargo test` green (132 baseline + 3 new = 135).
- `cargo clippy --all-targets -- -D warnings` green.
- `cargo fmt --all` clean.

## Notes for the agent

- This is **scoped to one file** — `src/tools/shell.rs`. Don't touch
  `src/tools/mod.rs`, `src/main.rs`, or the schema declared anywhere
  else.
- The current `run_shell` schema is the right anchor for the
  `apply_patch` — find the `parameters: json!({...})` literal inside
  `fn spec()` and extend it.
- Use `apply_patch`. `.to_string()` over `.into()` in tests.
- If multiple tests touch the same env var, **collapse into one
  sequential test** (AGENTS.md section 5 lesson from goal-23).
