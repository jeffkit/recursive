# Goal 12 — Smarter default system prompt

## What

Replace the current minimal `default_system_prompt()` in `src/config.rs`
with a slightly longer, opinionated version that nudges the agent
toward better-behaved defaults: prefer `apply_patch` for existing
files, run tests after changes, and stop calling tools when done.

Keep it short — well under 1 KB so it doesn't dominate the prompt
budget. Bias-by-default, not bias-by-overrule.

## Why

Goal 06 surfaced that ≈ 137:1 prompt:completion tokens come from
re-sending the transcript every step. The system prompt is only a
small fraction of that, but it sets the **shape** of behaviour we
see in every observation:

- MiniMax keeps reaching for `write_file` even on small files (see
  observation for goal 10: apply:write = 0:2 despite the goal saying
  use apply_patch). The default prompt doesn't tell it to prefer
  surgical edits.
- Neither model reliably runs tests after a code change unless the
  goal explicitly asks. The default prompt only says "after changes,
  run the project's tests" — true but vague.
- We don't tell the model what to do when it gets stuck. Adding
  "if a tool keeps failing the same way, try a different approach"
  is cheap behavioural insurance.

This goal hardens those defaults in one place so future runs don't
have to re-paste these instructions into every goal file.

## Scope (do exactly this, no more)

### 1. `src/config.rs`

Replace the body of `default_system_prompt()` with a new prompt.
The shape:

```rust
pub fn default_system_prompt() -> String {
    [
        "You are Recursive, a minimal but capable coding agent.",
        "",
        "Tools available: read_file, write_file, list_dir, run_shell, apply_patch, count_lines, search_files.",
        "All file paths are workspace-relative; the sandbox will reject anything outside.",
        "",
        "Working principles:",
        "- Read before you write. Skim relevant files (read_file, list_dir, search_files) before editing.",
        "- Prefer apply_patch over write_file when modifying existing files. Use write_file only for new files or full rewrites.",
        "- After any non-trivial code change, run the project's tests via run_shell and quote the result.",
        "- If a tool call fails the same way twice, change approach instead of retrying.",
        "- Stop calling tools and write a short final summary once the task is done.",
        "",
        "Output should be terse and concrete. Avoid filler.",
    ]
    .join("\n")
}
```

(Adjust the tool list if any of those names don't exist in the
current build — read `src/tools/` to confirm. `count_lines` and
`search_files` may not exist in some checkouts; just include the
ones that do.)

Keep the function signature unchanged: `pub fn default_system_prompt() -> String`.

### 2. Tests

In `src/config.rs`, add:

1. `default_prompt_is_well_under_a_kilobyte` — assert
   `default_system_prompt().len() < 1024`.
2. `default_prompt_mentions_apply_patch` — assert it contains
   `"apply_patch"`, since one of the goals is to nudge tool choice.
3. `default_prompt_mentions_run_shell_tests` — assert it contains
   both `"run_shell"` and the word `"tests"`.

These are pure string assertions; no async, no IO.

## Out of scope

- Per-model variations (e.g., a longer prompt for MiniMax). One
  default for everyone; users override via
  `--system-prompt-file` if they need bespoke behaviour.
- Parameterising the prompt at runtime (no template substitution).
- Editing `AGENTS.md` — that's a developer-side document, separate
  contract from the product's default prompt.
- Touching `src/main.rs` or `src/agent.rs`. Just `src/config.rs`.

## Definition of done

- `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test` all green.
- 3 new tests pass.
- `recursive run "hi"` still works (regression check: the prompt is
  not malformed JSON inside the chat completion).
- No new dependencies.

## Notes for the agent

- This is the smallest goal on file. The whole edit is in one
  function in one file. Use `apply_patch` for it.
- After patching `default_system_prompt`, run `cargo test` to confirm
  the new tests pass and the existing prompt tests still pass.
- Don't try to make the prompt clever or layered. It's a default;
  power users override it. Bias toward terse, not toward
  comprehensive.
