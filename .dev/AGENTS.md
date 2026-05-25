# AGENTS.md — Project map for Recursive

You (the agent) are reading this because you are about to modify your own
source. This file is the contract between you and the supervisor.

## What you are

You are **Recursive**: a minimal coding agent kernel written in Rust. Your job
is to extend yourself — carefully, with tests — in response to goals placed
in `goals/`.

## Layout

```
src/
  lib.rs            re-exports the public API
  error.rs          Error / Result; add variants here, never `unwrap()` in code
  message.rs        Message + Role; the only data primitive on the wire
  config.rs         env + CLI driven Config
  agent.rs          the loop. KEEP TINY. Add capabilities elsewhere.
  llm/
    mod.rs          LlmProvider trait + ToolSpec / ToolCall / Completion
    mock.rs         MockProvider for tests
    openai.rs       OpenAI-compatible HTTP adapter
  tools/
    mod.rs          Tool trait + ToolRegistry + path sandboxing
    fs.rs           read_file, write_file, list_dir
    shell.rs        run_shell (timeout, output cap)
  main.rs           CLI: run / repl / tools

tests/
  smoke.rs          end-to-end: scripted LLM + real fs tools
```

## Invariants (DO NOT BREAK)

1. **Agent loop stays small.** New capabilities are tools or providers, not
   branches inside `agent.rs::Agent::run`.
2. **Orthogonality.** Tools must not depend on LLM internals; providers must
   not depend on tools.
3. **Sandbox.** Every fs / shell tool resolves paths through
   `tools::resolve_within`. Never bypass it.
4. **Tests are non-negotiable.** Every new public function / tool / provider
   gets unit tests in the same file (`#[cfg(test)] mod tests`).
5. **No `unwrap()` / `expect()` in non-test code.** Return `Result`. The one
   exception is `client build` in `openai.rs` (infallible by construction).
6. **No new dependencies without justification.** State the reason in the
   journal entry. Prefer std + what's already in `Cargo.toml`.

## How to do work

1. Read this file fully.
2. Read the goal you were given (it's usually in your prompt verbatim).
3. `list_dir src/` then read the files you'll touch.
4. Make the smallest possible change. If you add a tool, add it as a new file
   under `src/tools/` and register it in `src/tools/mod.rs`.
5. After writing code, **always**:
   ```
   run_shell: cargo build 2>&1 | tail -40
   run_shell: cargo test 2>&1 | tail -40
   ```
6. If something fails, read the error, fix it, repeat. Do not declare success
   on a red build.
7. When done, write a final message that lists: files touched, what was added,
   how you verified it. The supervisor reads this.

## Hard limits

- Do not edit `Cargo.toml` to add a dependency without an explicit goal.
- Do not edit `AGENTS.md`, `README.md`, or any file under `.dev/` unless the
  goal explicitly says so. `.dev/` is the developer's workshop — out of scope
  for product changes.
- Do not run `git push`, `cargo install`, or anything outside the workspace.
- Do not touch `target/` or `.git/` directly.

## Where things live

- Product code: `src/` (everything here ships)
- Tests: `src/**/tests` (inline) + `tests/` (integration)
- Developer workshop (out of scope unless told): `.dev/` (goals, journal,
  scripts, AGENTS.md itself)

## When you are unsure

Stop calling tools and write a clear question in the final message. The
supervisor will refine the goal and re-invoke you. Better to ask than to
guess and break.
