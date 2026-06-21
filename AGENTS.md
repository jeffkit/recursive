# AGENTS.md — Working contract for AI agents in this repo

You are operating in the **recursive-agent** workspace. This is the
self-improving coding-agent project. The dev loop drives agents
(MiniMax / DeepSeek / GLM) to land roadmap features via
`.dev/scripts/self-improve.sh`. Detailed contract is in
`.dev/AGENTS.md` — read it before making changes.

## What you should know up front

- **Patch discipline matters.** Prefer `apply_patch` over `write_file`
  for edits to existing files. `write_file` is for new files. The
  observation system tracks `apply_patch:write_file` ratio and uses
  it to grade runs — high `write_file` count usually means
  `apply_patch` kept failing and the agent gave up.

- **V4A patch format** is the only `apply_patch` accepts (with some
  tolerance for unified-diff anchors). When in doubt, read
  `.dev/AGENTS.md` for the exact rules and common traps. Notable:
  context lines must be **unique**; if three lines in a row look
  identical to git, your patch will get rejected with "ambiguous".

- **Run `cargo test` after every product change.** `cargo run | jq`
  is NOT a substitute (build output pollutes stdout — see lesson 14
  in `.dev/AGENTS.md`). `cargo test` is the canonical verifier.

- **`cargo clippy --all-targets -- -D warnings` is enforced.** A
  clippy lint will cause `self-improve.sh` to roll back the entire
  product commit. Run clippy locally before declaring done.

- **Lint-as-you-go.** Use `cargo fmt --all` before committing.

## What's available besides the standard tools

If you see these tools in the registry list, you can use them:

- `apply_patch`, `read_file`, `write_file`, `list_dir`, `run_shell`
  — standard editing primitives.
- `search_files` (regex/case-insensitive supported) — fast in-tree
  search.
- `estimate_tokens` — budget planning before reading a large file.
- `web_fetch` — HTTP GET with HTML text extraction. Use sparingly;
  most goals don't need it.
- `remember` / `recall` / `forget` — persistent memory across runs,
  stored in `<workspace>/.recursive/memory/`. Use for facts you'll
  need next batch (e.g. "g42 cost record was $2.17, 45 patches").
- `load_skill` — discover and load detailed how-to skills from
  `<workspace>/.recursive/skills/` and `~/.recursive/skills/`. If
  the skill_index in your system prompt mentions a relevant skill,
  load it before doing related work.

If sub-agent is enabled (`RECURSIVE_SUBAGENT_ENABLED=1`):

- `sub_agent` — dispatch focused research/scan tasks to a fresh
  agent loop with restricted tools. Use for "summarize what AGENTS.md
  says about X" without polluting the main transcript.

If checkpointing is enabled (default when `git` is on PATH):

- `checkpoint_list` — list this session's per-turn workspace
  snapshots, newest first. Each turn produces one entry whose id
  you can pass to `checkpoint_diff`.
- `checkpoint_diff` — show a unified diff between two checkpoints
  in this session, or between a checkpoint and the current
  workspace. Useful for "what did I change in turn N?" self-checks.

You **cannot** create or restore checkpoints from inside the agent.
Snapshots are taken automatically by the runtime around every turn,
and rewinds happen out-of-band via
`recursive sessions rewind <session-id> --to-turn N`. Treat the
checkpoint chain as read-only telemetry about your own footprint.

## Don't surprise the orchestrator

- Each self-improve cycle has a step budget (default 200, hard cap
  200 single-pass × 2 with auto-resume = 400). Don't burn budget on
  exploratory reads. Plan first, then execute.

- `Stuck` detection trips on **three identical failing tool calls**.
  If you call `apply_patch` and it errors, change something
  (re-read context, widen anchors) before retrying — don't paste
  the same patch.

- Termination reasons (`BudgetExceeded`, `TranscriptLimit`,
  `Stuck`, `NoMoreToolCalls`) are **data, not errors**. Your
  transcript is saved on all of them. Don't panic.

- **Tool-call ↔ tool-result pairing.** If you write code that
  rearranges the transcript (compaction, trimming, replay, session
  resume), each `Role::Tool` message MUST stay immediately after the
  `Role::Assistant` message whose `tool_calls` lists its `id`.
  OpenAI/DeepSeek/Anthropic all reject orphans with HTTP 400. See
  invariant #8 in `.dev/AGENTS.md` and the regression test
  `compaction_keeps_tool_calls_paired_with_results`.

<!-- gitnexus:start -->
# GitNexus — Code Intelligence

This project is indexed by GitNexus as **Recursive** (9250 symbols, 21821 relationships, 300 execution flows). Use the GitNexus MCP tools to understand code, assess impact, and navigate safely.

> If any GitNexus tool warns the index is stale, run `npx gitnexus analyze` in terminal first.

## Always Do

- **MUST run impact analysis before editing any symbol.** Before modifying a function, class, or method, run `gitnexus_impact({target: "symbolName", direction: "upstream"})` and report the blast radius (direct callers, affected processes, risk level) to the user.
- **MUST run `gitnexus_detect_changes()` before committing** to verify your changes only affect expected symbols and execution flows.
- **MUST warn the user** if impact analysis returns HIGH or CRITICAL risk before proceeding with edits.
- When exploring unfamiliar code, use `gitnexus_query({query: "concept"})` to find execution flows instead of grepping. It returns process-grouped results ranked by relevance.
- When you need full context on a specific symbol — callers, callees, which execution flows it participates in — use `gitnexus_context({name: "symbolName"})`.

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
