# AGENTS.md — Working contract for AI agents in the recursive repo

> 本文件是 recursive 仓库的**唯一导航契约**：既讲如何**操作** agent（patch 格式、stuck 检测、
> 工具配对），也讲如何**修改** agent 源码（invariants、质量门、worktree、E2E 规则）。
> （`CLAUDE.md` 为本文件的软链，Claude Code 入口。）

You are operating in the **recursive-agent** workspace — the self-improving
Rust coding-agent project. `src/` is the product; `.dev/` is dev meta-tooling
(goals, scripts, roadmap). The dev loop drives agents (MiniMax / DeepSeek /
GLM) to land roadmap features via the Flowcast flow
`.dev/flows/self-improve.flow.js` (launched by `.dev/scripts/launch-flow.sh`;
see `.dev/flows/SELF_IMPROVE.md`). The legacy `.dev/scripts/self-improve.sh`
is deprecated. **Source-code invariants live in `.dev/AGENTS.md` — read it
before editing `src/`.**

## What you should know up front (操作契约)

- **Patch discipline.** Prefer `apply_patch` over `write_file` for edits to
  existing files (`write_file` = new files only). The observation system
  tracks the `apply_patch:write_file` ratio to grade runs.
- **V4A patch format** is the only `apply_patch` accepts (some tolerance for
  unified-diff anchors). Context lines must be **unique** — three identical
  lines in a row get rejected with "ambiguous". Read `.dev/AGENTS.md` for traps.
- **`cargo test` after every product change.** `cargo run | jq` is NOT a
  substitute (build output pollutes stdout — lesson 14 in `.dev/AGENTS.md`).
- **`cargo clippy --all-targets -- -D warnings` is enforced** — a lint rolls
  back the entire product commit in the self-improve flow.
- **`cargo fmt --all` before committing.**

## Before touching code (源码 invariants)

Read `.dev/AGENTS.md` for the full list. Especially:
- **#1** Agent loop stays small — don't branch inside `src/run_core.rs::RunCore::run_inner`.
- **#3** Sandbox — all fs/shell tools go through `tools::resolve_within`.
- **#5** No `unwrap()`/`expect()` in non-test code.
- **#7** Finish reasons are data, not errors.
- **#8** Tool-call ↔ tool-result pairing must be preserved.

New capabilities belong in tools (`src/tools/`) or providers (`src/llm/`),
**never** as a branch in `run_inner`. The legacy `src/agent.rs` was split into
`src/agent/types.rs` / `src/kernel.rs` / `src/runtime.rs` during Goal 219.

## Mandatory quality gates (before declaring done)

```bash
cargo test --workspace
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all
```

All three must be clean. If touching `crates/recursive-tui/src/`, also run
`.dev/scripts/tui-test-presence.sh` (hard gate) and `.dev/scripts/tui-mutants.sh`
(advisory for manual edits; full policy in `.dev/skills/tui-acceptance.md`).
The Flowcast self-improve flow enforces `tui-mutants` as a hard gate via
`.flowcast/gates.json` — that flow-level enforcement is intentional and unaffected
by the lighter manual-edit policy.

## Code conventions & after-change journal

- Minimal, surgical edits; don't rewrite a whole file to fix one thing.
- New tool → `src/tools/` + register in `src/tools/mod.rs`. New provider →
  `src/llm/` implementing `ChatProvider`. New error variant → `src/error.rs`.
- Tests → `#[cfg(test)] mod tests` in the same file.
- After changes, write `.dev/journal/manual-<YYYYMMDD>-<short-tag>.md`
  (Date / Goal / Files touched / Tests added / Notes) to keep observation
  history coherent with self-improve runs.

## Available tools (besides standard editing)

`apply_patch` / `read_file` / `write_file` / `list_dir` / `run_shell` /
`search_files` (regex) / `estimate_tokens` / `web_fetch` (sparingly) /
`remember`·`recall`·`forget` (memory in `<workspace>/.recursive/memory/`) /
`load_skill` (from `<workspace>/.recursive/skills/` or `~/.recursive/skills/`).

- **sub_agent** (if `RECURSIVE_SUBAGENT_ENABLED=1`): dispatch focused
  research/scan to a fresh agent loop with restricted tools.
- **checkpoint_list / checkpoint_diff** (if `git` on PATH): read-only per-turn
  workspace snapshots. You **cannot** create/restore checkpoints — rewinds
  happen out-of-band via `recursive sessions rewind <session-id> --to-turn N`.

## Don't surprise the orchestrator

- Step budget: default 200 (hard cap 400 with auto-resume). Don't burn it on
  exploratory reads — plan first.
- **Stuck** detection trips on **three identical failing tool calls**. Change
  something (re-read context, widen anchors) before retrying.
- Termination reasons (`BudgetExceeded` / `TranscriptLimit` / `Stuck` /
  `NoMoreToolCalls`) are **data, not errors** — transcript is always saved.
- **Tool-call pairing** (invariant #8): each `Role::Tool` MUST stay immediately
  after the `Role::Assistant` whose `tool_calls` lists its `id`. Orphans →
  HTTP 400. Regression test: `compaction_keeps_tool_calls_paired_with_results`.

## Worktree workflow

All feature development happens in a dedicated worktree at
`<project-root>/.worktrees/<name>/` (git-ignored). The main checkout (project
root) is reserved for the `main` branch — stable, non-bare, for shared admin
tasks (fetch, merge, housekeeping). This keeps the main checkout clean and
makes parallel feature work safe. Check before working:

```bash
ls .dev/runs/ 2>/dev/null
ls .worktrees/ 2>/dev/null
```

Don't edit files a live worktree run is working on.

### Known self-improve failure modes (treat as experimental)

1. **Auto-rollback can fail silently when the agent dies mid-fix** (auth/quota/
   malformed response). Always check `git -C .worktrees/<name> status` after a
   run — dirty tree means manual `git restore`.
2. **Cross-PR landing during a run creates phantom deletions.** Before merging
   an agent branch, rebase onto current `main`; `git log --oneline <branch>..main`
   shows intervening commits.
3. **`self-improve.sh` is deprecated — always use `parallel-self-improve.sh`**
   (which takes `<provider> <goal-file>`, handles concurrency, isolates worktrees,
   resumes on context loss).

New failure modes should be added here, not silently worked around.

## E2E testing rules (essentials — full detail in `e2e/` + `.dev/`)

E2E tests run via `argusai -c e2e.yaml`. Hard-won rules:
- **Confirm container binary first**: `docker exec recursive-e2e recursive --version`.
  Tool names are **PascalCase** (`Read`/`Write`/`Bash`/`Glob`) — snake_case
  assertions silently lie.
- **Port registry**: every HTTP suite picks a unique port (9090=08-http-api,
  9091=08b-rate-limit, 9092=18-goal-loop, 9093=19-interrupt, 9096=21-ts-sdk,
  9097=39-auth, 9099=22-compaction). Shared ports → 401/ECONNREFUSED ghosts.
- **Session isolation**: `RECURSIVE_SESSIONS_DIR` is a hard override ignoring
  `RECURSIVE_HOME`. For `recursive-session:` assertions, `unset RECURSIVE_SESSIONS_DIR`,
  use a unique `RECURSIVE_HOME`, then `find` the transcript and copy to a predictable
  path. See `e2e/tests/00-smoke.yaml` and `11-session-resume.yaml` for canonical form.
- **aimock fixtures**: use `turnIndex` + `hasToolResult`, not text matching.
- **HTTP in container**: always `-H 'Content-Type: application/json'`;
  `POST /sessions/:id/messages` field is **`content`** not `message`;
  use `http://127.0.0.1:PORT` not `localhost` (Node 18 IPv6 issue).
- **`recursive loop` produces no `transcript.jsonl`** — use `file:` assertions only.
- **`argusAI save:` can't capture exec stdout** — pass state via temp files.

## Skills available

- `/recursive-loop` — act as loop orchestrator: read roadmap, pick goals, launch
  the Flowcast self-improve flow, handle results. Use when the user wants
  Recursive to self-improve rather than you directly editing code.

<!-- gitnexus:start -->
# GitNexus — Code Intelligence

This project is indexed by GitNexus as **Recursive** (12664 symbols, 31434 relationships, 300 execution flows). Use the GitNexus MCP tools to understand code, assess impact, and navigate safely.

> If any GitNexus tool warns the index is stale, run `npx gitnexus analyze` in terminal first.

## Always Do

- **MUST run impact analysis before editing any symbol.** Before modifying a function, class, or method, run `gitnexus_impact({target: "symbolName", direction: "upstream"})` and report the blast radius (direct callers, affected processes, risk level) to the user.
- **MUST run `gitnexus_detect_changes()` before committing** to verify your changes only affect expected symbols and execution flows.
- **MUST warn the user** if impact analysis returns HIGH or CRITICAL risk before proceeding with edits.
- When exploring unfamiliar code, use `gitnexus_query({query: "concept"})` to find execution flows instead of grepping.
- When you need full context on a specific symbol, use `gitnexus_context({name: "symbolName"})`.

## Never Do

- NEVER edit a function, class, or method without first running `gitnexus_impact` on it.
- NEVER ignore HIGH or CRITICAL risk warnings from impact analysis.
- NEVER rename symbols with find-and-replace — use `gitnexus_rename` which understands the call graph.
- NEVER commit changes without running `gitnexus_detect_changes()` to check affected scope.

## Resources

| Resource | Use for |
|----------|---------|
| `gitnexus://repo/Recursive/context` | Codebase overview, check index freshness |
| `gitnexus://repo/Recursive/clusters` | All functional areas |
| `gitnexus://repo/Recursive/processes` | All execution flows |
| `gitnexus://repo/Recursive/process/{name}` | Step-by-step execution trace |

## CLI

| Task | Read this skill file |
|------|---------------------|
| Understand architecture / "How does X work?" | `.claude/skills/gitnexus/gitnexus-exploring/SKILL.md` |
| Blast radius / "What breaks if I change X?" | `.claude/skills/gitnexus/gitnexus-impact-analysis/SKILL.md` |
| Trace bugs / "Why is X failing?" | `.claude/skills/gitnexus/gitnexus-debugging/SKILL.md` |
| Rename / extract / split / refactor | `.claude/skills/gitnexus/gitnexus-refactoring/SKILL.md` |
| Tools, resources, schema reference | `.claude/skills/gitnexus/gitnexus-guide/SKILL.md` |
| Index, status, clean, wiki CLI commands | `.claude/skills/gitnexus/gitnexus-cli/SKILL.md` |

<!-- gitnexus:end -->
