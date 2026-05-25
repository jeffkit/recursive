# Goal 33 — `run_shell` stdin support

## Why

`run_shell` cannot pipe data into commands. To run `sort` on
arbitrary text, the agent has to either heredoc-into-shell (fragile
under quoting) or write a temp file first (extra step, leaves
artifacts). A structured `stdin: string` field makes piping data
in as a first-class operation.

## Scope

Touches: `src/tools/shell.rs` only (plus tests in the same file).

1. Extend the `run_shell` JSON schema in `spec()`:
   - Add an optional `stdin: string` parameter. Document it:
     "Optional UTF-8 bytes piped to the command's stdin. Defaults
     to empty (inherited tty-less stdin)."

2. In `execute()`:
   - Parse `stdin` from args (`args.get("stdin").and_then(|v|
     v.as_str())`).
   - When present, configure the spawned command with
     `Stdio::piped()` for stdin (existing implementation uses
     `Stdio::null()` or default), then write the bytes via
     `child.stdin.take().unwrap().write_all(stdin_bytes).await` —
     standard `tokio::process` pattern.
   - Drop the stdin handle before awaiting child exit (otherwise
     commands waiting for EOF will hang — `sort`, `cat`, `wc`).

3. Tests in the same file:
   - **Test A**: pipe `"banana\napple\ncherry\n"` into `sort` and
     assert stdout contains the three words in sorted order.
   - **Test B**: pipe `"hello"` into `cat`; assert stdout == "hello".
   - **Test C** (regression): omitting `stdin` works exactly as
     before (use the existing test pattern as your reference).

## Acceptance

- `cargo build` green.
- `cargo test` green (138 baseline + 3 new = 141).
- `cargo clippy --all-targets -- -D warnings` green.
- `cargo fmt --all` clean.

## Notes for the agent

- This is **scoped to one file** — `src/tools/shell.rs`. Don't
  touch `tools/mod.rs`, `main.rs`, or anything else.
- The existing test infrastructure for shell.rs already runs real
  commands (`echo`, `pwd`). Use that pattern; don't mock.
- Beware: tests that touch stdin must NOT use `cargo test
  --test-threads=1` to "fix" race conditions — those are tests
  about isolated subprocess behavior, no env-var sharing involved.
- Use `apply_patch`. `.to_string()` over `.into()` in tests.
