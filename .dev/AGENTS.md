# AGENTS.md — Project map for Recursive

> **Two files, two audiences — read both.**
> - **This file** (`.dev/AGENTS.md`) — source-code invariants. Read
>   before editing `src/`. Documents the 8 invariants the kernel and
>   run loop depend on, the current module layout, and the quality
>   gates that must pass before a commit lands.
> - **`/AGENTS.md`** (project root) — short runtime contract Recursive
>   injects into its own system prompt at v0.7 (loaded by
>   `src/config.rs::load_project_context`). Says how to *operate* the
>   agent: patch format, stuck detection, tool-call pairing. Useful
>   background when you're changing the prompt, tool registry, or
>   stuck-detection logic.

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
  agent/types.rs    FinishReason, PermissionDecision, PermissionHook, PlanningMode
                    (the legacy src/agent.rs was split in Goal 219 — the runtime
                    loop now lives in kernel.rs + run_core.rs, see below)
  kernel.rs         stateless single-turn ReAct executor (AgentKernel + TurnContext)
  run_core.rs       the ReAct step loop itself (RunCore::run_inner).
                    KEEP TINY. New capabilities go in tools/, not as new
                    branches in run_inner. This is the file Invariant #1
                    protects.
  runtime.rs        stateful wrapper (AgentRuntime) — transcript, checkpoints,
                    goal state, message queue, cross-turn compaction
  runtime_goal.rs   GoalState / GoalStatus / GoalEvaluator (auto-loop judge)
  coordinator.rs    coordinator-mode orchestrator (multi-agent dispatch)
  multi.rs          multi-agent pool, shared memory, message bus
  compact.rs        LLM-driven transcript compaction
  checkpoint.rs     git-backed shadow-repo snapshots + restore
  transcript.rs     on-disk transcript (jsonl) reader/writer
  session/          session lifecycle, persistence, resume, orphan cleanup
  hooks/            lifecycle hook registry + external hook runner
  permissions/      layered permissions + auto-classifier
  memory/           scratchpad + vector memory (sqlite_vec / openai_embedding)
  storage/          storage backends (local / s3 / redis)
  http/             axum HTTP API + SSE + auth + rate-limit (feature = "http")
  mcp.rs            MCP client (model context protocol)
  mcp_server.rs     expose tools as an MCP server (feature = "mcp")
  skills.rs         skill discovery + injection
  llm/
    mod.rs          ChatProvider trait + ToolSpec / ToolCall / Completion
    openai.rs       OpenAI-compatible HTTP adapter
    anthropic.rs    Anthropic native API adapter
    mock.rs         MockProvider for tests
    search.rs       deferred-tool search engine (software ToolSearch)
    pricing.rs      cost tracking
  tools/
    mod.rs          Tool trait + ToolRegistry + path sandboxing
    dispatch.rs     invoke_with_audit + touched-file recording + sandbox roots
    registry.rs     ToolRegistry state, permissions, hooks, classifier
    fs.rs           Read, Write, Glob
    shell.rs        Bash (timeout, output cap, kill_on_drop)
    edit.rs         str_replace Edit tool
    agent.rs        AgentTool (sub-agent spawn) + shared memory bridge
    a2a.rs          A2A (agent-to-agent) protocol tools
    checkpoint.rs   checkpoint_save / checkpoint_list / checkpoint_diff
    episodic_recall.rs   search past sessions
    facts.rs        RememberFact / RecallFact / FactStore
    memory.rs       Scratchpad tools + WorkingMemory
    send_message.rs WorkerMailbox (coordinator ↔ worker)
    plan_mode.rs    EnterPlanMode / ExitPlanMode / approval gates
    todo.rs         TodoWriteTool (task list)
    web_fetch.rs / web_search.rs     web tools (feature-gated)
    permission_pipeline.rs / policy_sandbox.rs / audit.rs
                    pre-execution permission orchestration + audit records
    task_*.rs / team_*.rs   coordinator-mode task/team tools
    transport.rs    pluggable transport (local / docker / e2b / ssh)
    docker_sandbox.rs / docker_provider.rs / e2b_provider.rs
                    sandboxed Bash providers (feature-gated)
    run_background.rs       background-job Bash manager
  main.rs / crates/recursive-cli   CLI: run / repl / tools / loop / http / mcp
  crates/recursive-tui             ratatui TUI
  crates/agui-{protocol,client,tui}   AG-UI protocol stack

tests/
  invariants/       the 8 invariant tests (loop_size, sandbox, pairing, ...)
  smoke.rs          end-to-end: scripted LLM + real fs tools
  http.rs           HTTP API integration tests
  http_common/      shared fixtures for HTTP tests

e2e/
  tests/            container-based end-to-end scenarios (argusai)
```

## Invariants (DO NOT BREAK)

1. **Agent loop stays small.** New capabilities are tools or providers, not
   branches inside `src/run_core.rs::RunCore::run_inner`.
   Automated test: `tests/invariants/loop_size_orthogonality.rs` (invariant #1)
2. **Orthogonality.** Tools must not depend on LLM internals; providers must
   not depend on tools.
   Automated test: `tests/invariants/loop_size_orthogonality.rs` (invariant #2)
3. **Sandbox.** Every fs / shell tool resolves paths through
   `tools::resolve_within`. Never bypass it.
   Automated test: `tests/invariants/sandbox.rs`
4. **Tests are non-negotiable.** Every new public function / tool / provider
   gets unit tests in the same file (`#[cfg(test)] mod tests`).
   Automated test: `tests/invariants/test_coverage.rs`
5. **No `unwrap()` / `expect()` in non-test code.** Return `Result`. The one
   exception is `client build` in `openai.rs` (infallible by construction).
   Enforced by: `clippy::unwrap_used` deny (added in Goal 224)
6. **No new dependencies without justification.** State the reason in the
   journal entry. Prefer std + what's already in `Cargo.toml`.
   Automated test: `tests/invariants/dep_justification.rs` +
   `scripts/check-new-deps.sh`
7. **Finish reasons are data, not errors.** `Agent::run` returns
   `Ok(AgentOutcome { finish: ... })` for every termination mode
   (`NoMoreToolCalls`, `BudgetExceeded`, `Stuck`, `TranscriptLimit`,
   `ProviderStop`). Only honest-to-god failures (network, JSON,
   provider transport, IO) become `Err`. The CLI decides binary
   exit code by inspecting `outcome.finish` AFTER persisting the
   transcript — see `main.rs::exit_for_finish`. **NEVER** introduce
   a new `Error::XxxBudget` or `Error::XxxLimit` variant that
   short-circuits the transcript save. The self-improve flow's auto-resume
   step depends on the saved transcript existing on disk.
   Automated test: `tests/invariants/finish_reason_data.rs`
8. **Tool-call ↔ tool-result pairing.** Every `Role::Tool` message
   in the transcript MUST be immediately preceded by a `Role::Assistant`
   message whose `tool_calls` contains the matching id. OpenAI,
   DeepSeek, and Anthropic all enforce this server-side (HTTP 400
   "Messages with role 'tool' must be a response to a preceding
   message with 'tool_calls'"). Any operation that mutates the
   transcript mid-run — compaction, trimming, splicing, resume
   replay — MUST preserve this invariant or rebase the window past
   the orphan. Discovered via batch 15 dogfood: a naive
   `keep_recent_n=N` split in `agent::Agent::maybe_compact` orphaned
   a tool result whose parent assistant had just been drained. Fix:
   retreat the split until `transcript[split].role != Role::Tool`.
   Automated test: `tests/invariants/tool_call_pairing.rs`

## How to do work

1. Read this file fully.
2. Read the goal you were given (it's usually in your prompt verbatim).
3. `Glob src/` then read the files you'll touch.
4. Make the smallest possible change. If you add a tool, add it as a new file
   under `src/tools/` and register it in `src/tools/mod.rs`.
5. **Prefer `Edit` over `Write`** for any change to a file you
   didn't just create. `Write` overwrites the entire file and risks
   silently dropping unrelated code; `Edit` requires you to quote
   the context you're editing, which catches drift early.
   - Use `Write` only for: brand-new files, or whole-file rewrites
     when you have read the entire current contents and intentionally
     want to replace them.
   - **V4A patch format — read this carefully, it is NOT unified diff:**
     - The `@@` lines are **optional anchors** containing a *unique line of
       source code* that already exists in the file. They disambiguate
       when the same context block appears more than once. They are NOT
       hunk headers with line numbers. Both `@@ <anchor>` and
       `@@ -N,M +N,M @@ <anchor>` are accepted; the line-number range,
       when present, is ignored. What matters is the anchor text after
       the final `@@` and the byte-for-byte context lines that follow.
     - Each `*** Update File: <path>` block must appear AT MOST ONCE per
       patch. To make multiple edits to the same file, put multiple hunks
       (each optionally preceded by its own `@@ anchor`) inside one
       `*** Update File:` block.
     - **Common Rust trap in tests:** when a constructor signature is
       `fn user(s: impl Into<String>)`, writing `Message::user("foo".into())`
       in a test gives the compiler no way to choose the `.into()` target
       and you get a *type-annotation needed* error. Use
       `Message::user("foo".to_string())` instead. The agent that wrote
       goal-17 burned its anti-stuck budget on exactly this — three
       identical patch retries because the unique-context rule of V4A
       can't disambiguate three near-identical lines.
     - **Env-var tests must be ONE test, not many.** `cargo test` runs
       tests in parallel threads by default. `std::env::set_var` and
       `remove_var` are process-global, so two tests touching the same
       `RECURSIVE_*` variable will race — one sees the other's value
       intermittently, no amount of "save/restore" inside each test
       fixes it. Collapse defaults + override checks into a single
       sequential test. Goal-23's MiniMax run burned all 50 steps
       trying to debug this race; the consolidated test pattern in
       `src/config.rs::shell_timeout_default_and_env_override` is the
       reference. See also `retry_env_overrides_apply` (one test that
       toggles all retry vars at once).
     - **Network tests must set explicit timeouts.** `reqwest::Client`
       has NO default connect timeout and NO default request timeout.
       A test that connects to an unreachable address (e.g.
       `http://127.0.0.1:1` where the OS silently drops SYN packets)
       will hang `cargo test` *forever*, holding the build lock and
       deadlocking every subsequent `cargo test` invocation. Always
       build provider clients in tests with
       `.timeout(std::time::Duration::from_secs(2))` (request) AND
       `.connect_timeout(std::time::Duration::from_secs(1))`
       (connect). Goal-30 burned 5 wall-clock minutes deadlocked on
       three concurrent hung `cargo test` processes before the
       orchestrator manually killed them. If a test legitimately
       needs to assert "this hangs", do it with
       `tokio::time::timeout(...)` wrapping the call, not by letting
       reqwest run unbounded.
     - Worked example, editing `src/llm/mod.rs` to add a struct after the
       `pub use openai::OpenAiProvider;` line:
       ```
       *** Begin Patch
       *** Update File: src/llm/mod.rs
       @@ pub use openai::OpenAiProvider;
        pub use openai::OpenAiProvider;

       +/// New thing.
       +pub struct NewThing;
       +
        /// JSON-schema description of a tool, sent verbatim to the model.
       *** End Patch
       ```
       Note: the `@@` line cites an existing line of code; the lines after
       it that start with a space are unchanged context that must match
       the file byte-for-byte; `+` adds; `-` removes.
6. After writing code, **always**:
   ```
   Bash: cargo build 2>&1 | tail -40
   Bash: cargo test 2>&1 | tail -40
   Bash: cargo fmt --all
   Bash: cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -40
   ```
   All four must be green before you stop. `-D warnings` means even a
   clippy *warning* will fail the build.

   **`cargo fmt --all` is enforced as a hard gate by the self-improve flow**
   (since g141): if `cargo fmt --all -- --check` is non-zero after your
   edits, the flow rolls the run back (the fmt gate uses `onFail: autofix`,
   so it first runs `cargo fmt --all` in place, then re-checks). Only set
   `RECURSIVE_FMT_CHECK=0` if you have a documented reason in the
   journal entry.

   **`cargo clippy --all-targets --all-features -- -D warnings` is also
   a hard gate by the self-improve flow** (since g262 — added after a
   deepseek-pro run landed 2 unused imports that `cargo test` accepted
   but `cargo clippy` rejected): if the clippy run is non-zero after
   your edits, the flow invokes a one-shot resume-fix replay
   asking you to clean up the lints, then re-runs the gate. A
   mechanical lint (needless_borrow, redundant_clone, unused_imports)
   is almost always a one-line change — do not push back. Only set
   `RECURSIVE_CLIPPY_CHECK=0` if a goal genuinely needs to land
   clippy-dirty code (very rare; document the reason in the journal
   entry).

   **E2E smoke is a hard gate by the self-improve flow** (restored after
   being silently skipped on g262): the flow runs the `e2e` project gate
   declared in `.flowcast/gates.json` →
   `cd e2e && argusai -c e2e.yaml run -s smoke` (3 scenarios: basic
   Write, basic Read, session-recording assertions).
   Replay mode — deterministic, no API key, ~700ms. If it fails the
   flow invokes a one-shot resume-fix replay asking you to fix
   the regression. If the E2E prerequisites are missing (argusai not
   on PATH, no `e2e/e2e.yaml`, or `e2e/plugins/dist/index.js` not
   built) the gate is HARD-FAIL — the flow rolls back. argusai
   is normally picked up via fnm's multishell path, but the gate
   has a fallback to the stable fnm install path
   (`$FNM_DIR/node-versions/*/installation/bin/argusai`) for
   non-interactive subprocesses. Only set `RECURSIVE_SMOKE_TEST=0`
   if Docker is genuinely unavailable in the run environment.

   **TUI mutation gate is a hard gate by the self-improve flow** (added in the
   tui-test review): when a goal changes anything under
   `crates/recursive-tui/src/`, the flow's `tui-mutants` project gate
   (declared in `.flowcast/gates.json`) runs
   `.dev/scripts/tui-mutants.sh` (scoped to the changed files) after the
   e2e gate. A surviving mutant = a test that passes but doesn't pin the
   changed behaviour → `onFail: resume-fix` (the flow feeds the survivor
   report back to the agent to strengthen tests, then re-runs the gate);
   still failing → rollback. `cargo-mutants` missing is also a hard
   failure — install it (`cargo install cargo-mutants`). `tui-mutants.sh`
   self-skips (exit 0) when no TUI source changed, so non-TUI goals pay
   nothing. The legacy `.dev/scripts/self-improve.sh` is deprecated and
   does NOT carry this gate — use the flow.
   The full SOP is `.dev/skills/tui-acceptance.md`; the in-process harness
   is `crates/recursive-tui/src/harness.rs`.

   **TUI changes must land tests in the same change** (contract, not just
   gate). Any edit to `crates/recursive-tui/src/` MUST ship a test covering
   the new/changed behaviour in the SAME commit:
   - in-process: a `#[cfg(test)] mod tests` block in the changed file
     (`use crate::harness::Harness;`), asserting via `Screen::find_row` /
     `row_has_bg_color` / `text()` — NOT internal-state peeks;
   - or integration: a case under `crates/recursive-tui/tests/`;
   - or PTY: a case in `crates/tui-pty-harness/` for terminal-IO behaviour
     the in-process harness can't reach.
   Two flow gates enforce this: `tui-presence` (fast — fails if TUI src
   changed with no test-bearing addition; `RECURSIVE_TUI_TEST_PRESENCE=0`
   opt-out for a pure refactor, documented in the journal) runs BEFORE
   `tui-mutants` (slow — rejects tests that pass but don't pin behaviour).
   Write the test as you write the code, not after the gate fires — the
   mutation gate will catch tautological tests, costing a resume-fix cycle.

   **E2E test authoring rules (applies when a goal touches `e2e/`).**
   The container binary (`recursive-e2e`) may lag the source tree. Before
   writing E2E assertions, verify the actual tool names and env-var support:
   ```bash
   docker exec recursive-e2e sh -c \
     'echo "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\",\"params\":{}}" \
      | recursive mcp' | jq '[.result.tools[].name]'
   ```
   Key invariants:
   - Tool names are PascalCase (`Read`, `Write`, `Bash`, `Glob`). Don't assert
     `read_file` / `write_file` — those are backward-compat aliases that may
     disappear.
   - `RECURSIVE_SESSIONS_DIR` is ignored by older binaries. For any suite
     that uses `recursive-session:` assertions, isolate with
     `RECURSIVE_HOME=/tmp/rh-{unique}` and dynamically `find transcript.jsonl`
     to copy it to a predictable path. Never hardcode a sessions path.
   - `POST /sessions` requires `Content-Type: application/json`. The messages
     endpoint field is `content`, not `message`.
   - Node.js scripts: use `http://127.0.0.1:PORT`, not `http://localhost:PORT`
     (Node 18 resolves localhost → ::1 IPv6; server binds IPv4 only).
   - `recursive loop` does not produce `transcript.jsonl`; only use `file:`
     assertions for loop-mode tests.
   Full details: `CLAUDE.md` → "E2E testing rules" section.

   **Verify behavior through `cargo test`, never through `cargo run | jq`.**
   On a fresh worktree, `cargo run` first does a full `cargo build`, whose
   "Compiling …" / "Finished …" lines spill onto stderr *and sometimes
   stdout*. Piping that into `jq` blows up with `parse error: Invalid
   numeric literal`, sends you on a multi-step debugging detour, and burns
   the step budget. Two prior runs were rolled back this exact way.

   If you need to assert on JSON / CLI output shape, write a unit test
   (e.g. `serde_json::from_str(&serialized).unwrap()` round-trip in
   `#[cfg(test)] mod tests`). Tests run against a pre-built binary, give
   structured pass/fail, and cost one tool call.

   Common fix patterns:
   - `clippy::should_implement_trait` (method named `add` / `sub` / …):
     either rename the method (e.g. `add` → `accumulate`) or implement the
     corresponding `std::ops` trait. Both are acceptable.
   - `clippy::needless_borrow`, `clippy::redundant_clone`: just apply the
     suggested fix; these are mechanical.
7. If something fails, read the error, fix it, repeat. Do not declare success
   on a red build.
8. When done, write a final message that lists: files touched, what was added,
   how you verified it. The supervisor reads this.

## Hard limits

- Do not edit `Cargo.toml` to add a dependency without an explicit goal.
- Do not edit `AGENTS.md`, `README.md`, or any file under `.dev/` unless the
  goal explicitly says so. `.dev/` is the developer's workshop — out of scope
  for product changes.
- Do not run `git push`, `cargo install`, or anything outside the workspace.
- Do not touch `target/` or `.git/` directly.
- **Do not modify source files via shell tricks.** Specifically, never use
  `head` / `tail` / `cat heredoc` / `sed -i` / `mv` to rewrite or splice a
  file under `src/` or `tests/`. They look surgical but routinely truncate
  files mid-block, leaving unclosed `{` or unterminated strings. Always use
  `Edit` (preferred) or `Write` (whole file, contents provided
  in one call). Both are atomic; shell pipelines are not.
- **Never run `git` against the working tree.** No `git checkout`, no
  `git reset`, no `git restore`, no `git stash`. The wrapper script owns
  rollback; if you try to "undo" yourself you will silently destroy your
  own in-progress work and lose the run. If you painted yourself into a
  corner, write a final message describing the situation and stop — the
  supervisor will roll you back cleanly.

## Where things live

- Product code: `src/` (everything here ships)
- Tests: `src/**/tests` (inline) + `tests/` (integration)
- Developer workshop (out of scope unless told): `.dev/` (goals, journal,
  scripts, AGENTS.md itself)

## When you are unsure

Stop calling tools and write a clear question in the final message. The
supervisor will refine the goal and re-invoke you. Better to ask than to
guess and break.
