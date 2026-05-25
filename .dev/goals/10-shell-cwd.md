# Goal 10 — Optional `cwd` argument for `run_shell`

## What

Let the `run_shell` tool accept an optional `cwd` (current working
directory) argument expressed as a workspace-relative path. If
provided, the command runs in that subdirectory; if not, it falls
back to the workspace root (today's behaviour).

The new `cwd` must be confined to the workspace via the existing
`resolve_within` sandbox helper. Anything escaping the root must
return a clear `Error::BadToolArgs`.

## Why

Real agent runs frequently need to execute commands inside a
subdirectory (`cargo test` inside a workspace member, `npm install`
inside a frontend folder, etc.). Today the only workarounds are:

- `run_shell { "command": "cd subdir && cmd" }` — ugly, and parsing
  shell `cd …` text would be the wrong layer to do sandbox checks.
- Running everything from root with relative paths — fragile and
  forces awkward command lines.

A first-class `cwd` is one extra optional argument, fits in the
existing sandbox model, and turns a common pattern into a normal
tool call.

## Scope (do exactly this, no more)

### 1. `src/tools/shell.rs`

Update the `Tool` impl for `RunShell`:

- Add `cwd` to the JSON-schema in `spec()`:
  ```jsonc
  "cwd": {
    "type": "string",
    "description": "Optional subdirectory (relative to workspace root) to run the command in. Must stay inside the workspace."
  }
  ```
  Do not add `cwd` to `required`.

- In `execute(...)`:
  - Read `args.get("cwd").and_then(|v| v.as_str())`.
  - If `Some(rel)`, call the existing `crate::tools::resolve_within(&self.root, rel)`
    helper (the same one `read_file` / `write_file` already use).
    On error from `resolve_within`, return `Err(Error::BadToolArgs { name: "run_shell", message: format!("cwd: {e}") })`.
  - If `None`, keep the existing behaviour (run in `self.root`).
  - Pass the resolved path to `cmd.current_dir(...)`.

- Update the existing `description` in `spec()` to mention the new
  argument briefly: e.g.
  `"Run a shell command (sh -c) from the workspace root, or from an optional subdirectory inside it via `cwd`."`

### 2. Tests

Add to the `#[cfg(test)] mod tests` block:

1. `runs_in_subdir_when_cwd_given` — create a temp dir, make a
   subdirectory `sub` inside it with a marker file `marker.txt`,
   run with `cwd="sub"` + `command="ls"`, assert output contains
   `marker.txt`.
2. `rejects_cwd_outside_workspace` — try `cwd="../escape"`, assert
   the result is `Err(Error::BadToolArgs { .. })` mentioning `cwd`.
3. `accepts_dot_cwd_as_root` — `cwd="."` is equivalent to no `cwd`;
   `pwd` output should still be inside the workspace (substring
   match against the canonicalised root is acceptable, but a
   simpler check is fine: just `exit: 0` plus output is non-empty).
4. `existing_no_cwd_call_still_works` — call without `cwd` at all,
   assert it still runs from the root.

Use the existing `tempfile::TempDir` pattern from the file. Don't
add new test-time deps.

## Out of scope

- Absolute `cwd` paths. Reject them like any other out-of-root
  path; that's what `resolve_within` already does.
- Environment-variable forwarding from the tool args.
- Per-call timeout override (already exists at builder level).
- Anything outside `src/tools/shell.rs` (no agent-loop changes,
  no CLI flag — this is a tool-internal improvement).

## Definition of done

- `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test` all green.
- 4 new tests pass; 3 existing tests still pass.
- The agent driving itself with `RECURSIVE_PROVIDER=…` against this
  goal could call `run_shell` with `{"command": "ls", "cwd": "src"}`
  and see only `src/` contents.
- No new dependencies; `resolve_within` is reused, not duplicated.

## Notes for the agent

- `resolve_within` lives in `src/tools/mod.rs`. Check its signature
  before calling — it already returns a sensible `Result<PathBuf>`.
- The change inside `execute(...)` is roughly six lines: read the
  optional arg, resolve it on `Some`, pick the directory to pass
  to `current_dir`. Use `apply_patch` for the file edit; the file
  is small so anchors will be easy.
- Be careful with the new test for `"."`: `resolve_within` may
  canonicalise it, which on macOS prepends `/private/...`. Don't
  assert the exact path; just assert the command succeeded.
- Don't change the existing struct fields or the public API. Just
  extend behaviour through the JSON schema.
