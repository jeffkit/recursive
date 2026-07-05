# CLAUDE.md — Recursive Project

This file governs how Claude Code (you) behaves when directly editing
the Recursive codebase. These rules override defaults.

## What this project is

Recursive is a self-improving Rust coding agent. The source in `src/`
is the product. `.dev/` is the development meta-tooling (goals, scripts,
roadmap). You are editing the product, not the meta-tooling, unless
explicitly asked otherwise.

## Before touching any code

1. Read `.dev/AGENTS.md` — the full invariant list. Especially:
   - Invariant #1: Agent loop stays small. Don't branch inside `src/run_core.rs::RunCore::run_inner`.
   - Invariant #3: Sandbox. All fs/shell tools go through `tools::resolve_within`.
   - Invariant #5: No `unwrap()`/`expect()` in non-test code.
   - Invariant #7: Finish reasons are data, not errors.
   - Invariant #8: Tool-call ↔ tool-result pairing must be preserved.

2. Check which files your change touches. If you're touching the kernel
   (`src/kernel.rs`) or the ReAct step loop (`src/run_core.rs::RunCore::run_inner`),
   reconsider — new capabilities belong in tools or providers. The legacy
   `src/agent.rs` was split into `src/agent/types.rs` (FinishReason etc.),
   `src/kernel.rs` (stateless executor), and `src/runtime.rs` (stateful wrapper)
   during Goal 219.

## Mandatory quality gates (run before declaring done)

```bash
cargo test --workspace
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all
```

All three must be clean. If clippy has warnings, fix them — the self-improve
script treats clippy failures as rollback triggers.

If your change touches `crates/recursive-tui/src/`, also run the TUI
test gates and the PTY tour described in `.dev/skills/tui-acceptance.md`:

```bash
.dev/scripts/tui-test-presence.sh    # fast: must exit 0 (you added tests)
.dev/scripts/tui-mutants.sh          # must exit 0 (no surviving mutants)
```

The presence gate fails if TUI src changed with no test-bearing addition
(`RECURSIVE_TUI_TEST_PRESENCE=0` opt-out for a pure refactor — document
why). The mutation gate fails if tests pass but don't pin behaviour.
`self-improve.sh` enforces these as hard gates (`RECURSIVE_TUI_MUTANTS`),
so a TUI change that lands weak tests gets rolled back. The canonical
self-improve path is the Flowcast flow (`.dev/flows/self-improve.flow.js`);
its `tui-presence` and `tui-mutants` project gates (declared in
`.flowcast/gates.json`) run `tui-test-presence.sh` then `tui-mutants.sh`
with `onFail: resume-fix`, so missing/weak tests are fed back to the
agent. `self-improve.sh` is deprecated —
see the warning at its top.

## Code conventions

- **Prefer `Edit` discipline mentally**: when editing existing files,
  make minimal, surgical changes. Don't rewrite a whole file to fix one thing.
- **New tool** → new file under `src/tools/`, register in `src/tools/mod.rs`.
- **New provider** → new file under `src/llm/`, implement `ChatProvider` trait.
- **New capability** → never add it as a branch inside `src/run_core.rs::RunCore::run_inner`.
- **Error variants** → add to `src/error.rs`. Never `unwrap()` in product code.
- **Tests** → `#[cfg(test)] mod tests` in the same file. Every new public
  function/tool/provider gets unit tests.

## After making changes

Write a brief journal entry under `.dev/journal/` in the format:
`manual-<YYYYMMDD>-<short-tag>.md`

```markdown
# Manual edit: <tag>

**Date**: YYYY-MM-DD
**Goal**: <what you changed and why>
**Files touched**: <list>
**Tests added**: <list or "none">
**Notes**: <anything non-obvious>
```

This keeps the observation history coherent with the self-improve loop runs.

## Parallel workflow context

This project also uses Recursive's **self-improve loop** (orchestrated via
the Flowcast flow at `.dev/flows/self-improve.flow.js`, launched by
`.dev/scripts/launch-flow.sh`; see `.dev/flows/SELF_IMPROVE.md`) where
Recursive edits its own source. The legacy `.dev/scripts/self-improve.sh`
is deprecated. If you're about to do work that could conflict with an
in-flight self-improve run, check first:

```bash
ls .dev/runs/ 2>/dev/null
ls .worktrees/ 2>/dev/null
```

Don't edit files that a live worktree run is working on.

### Known self-improve failure modes (treat as experimental)

The flow is *not* a fully reliable pipeline. Three failure modes have
been observed in production and are not yet fixed — assume any of these
can happen on a given run, and design your workflow around them rather
than treating the green-path as guaranteed.

1. **Auto-rollback can fail silently when the agent dies mid-fix.** If
   the agent crashes from an unrecoverable LLM error (auth, quota,
   malformed provider response), the worktree may be left in a dirty
   state and the flow's rollback step never runs. **Always check
   `git -C .worktrees/<name> status` after a run before assuming the
   rollback succeeded.** A dirty tree means you must `git restore`
   manually.

2. **Cross-PR landing during a run creates phantom deletions.** When
   the user (or another agent) merges a PR to `main` while a self-improve
   run is in flight on a branch forked from old `main`, the eventual
   merge of the agent branch will appear to delete files that the
   cross-PR added. **Before merging an agent branch, rebase it onto
   current `main`** so the diff is computed against the up-to-date
   tree. `git log --oneline <agent-branch>..main` shows the
   intervening commits.

3. **`self-improve.sh` is deprecated — always use `parallel-self-improve.sh`.**
   The legacy script does not handle concurrent runs, does not isolate
   per-goal worktrees, and does not resume cleanly on context loss. The
   argument order also differs: `parallel-self-improve.sh` takes
   `<provider> <goal-file>` (provider first, then goal). The legacy
   script is kept only for archaeological reference and may be removed.

These three are *known*. New failure modes should be added here as
they're discovered, not silently worked around.

## Worktree workflow

All feature development happens in a dedicated worktree, not on the main
checkout at the project root. The main checkout (the project root itself)
is reserved for the `main` branch — it is the stable, non-bare working
tree used for shared admin tasks (fetch, merge, housekeeping). Each
feature worktree lives at `<project-root>/.worktrees/<name>/`, and
`.worktrees/` is git-ignored so worktrees never get accidentally
committed.

This separation keeps the main checkout clean, makes parallel feature
work safe, and prevents in-flight changes from colliding with the
stable branch. A worktree is a full working tree on a different branch,
so editing one does not touch the other.

## E2E testing rules

E2E tests live in `e2e/` and run via `argusai -c e2e.yaml`. Before writing
or modifying any E2E test, internalize these hard-won rules:

### Before writing a new suite

1. **Confirm the container binary first.** The binary inside `recursive-e2e`
   may lag the source tree. Always check before writing assertions:
   ```bash
   docker exec recursive-e2e recursive --version
   docker exec recursive-e2e sh -c 'echo "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\",\"params\":{}}" | recursive mcp' | jq '[.result.tools[].name]'
   ```
   Tool names are **PascalCase** (`Read`, `Write`, `Bash`, `Glob`). If you
   assert snake_case (`read_file`, `write_file`) the test will silently lie.

2. **Port registry — every HTTP suite picks a unique port:**

   | Port | Suite |
   |------|-------|
   | 9090 | 08-http-api |
   | 9091 | 08b-http-rate-limit |
   | 9092 | 18-goal-loop |
   | 9093 | 19-http-interrupt |
   | 9096 | 21-typescript-sdk |
   | 9097 | 39-http-auth |
   | 9098 | (reserved) |
   | 9099 | 22-compaction |

   Add new HTTP suites to this table. Shared ports cause 401/ECONNREFUSED ghosts.

### Session path isolation (mandatory for `recursive-session:` assertions)

`RECURSIVE_SESSIONS_DIR` is a **hard override** (`src/paths.rs::user_sessions_dir`):
when set, sessions land exactly there, ignoring `RECURSIVE_HOME`. The
`recursive-e2e` container sets `RECURSIVE_SESSIONS_DIR=/workspace/sessions`
(see `e2e/e2e.yaml`), so a bare `RECURSIVE_HOME=/tmp/rh-... recursive run`
writes the session to `/workspace/sessions`, **not** under `RECURSIVE_HOME` —
and a subsequent `find /tmp/rh-...` silently finds nothing, failing the
assertion with "No session directory found".

Required pattern for every agent `run` that needs a `recursive-session:` assertion:

```bash
# 1. Unset the container-wide override AND isolate with a unique RECURSIVE_HOME.
#    Without `unset`, the session lands at /workspace/sessions and the find below misses it.
unset RECURSIVE_SESSIONS_DIR
RECURSIVE_HOME=/tmp/rh-mytest recursive run --max-steps 3 ...

# 2. Dynamically locate the transcript
SESSION_DIR=$(find /tmp/rh-mytest -name "transcript.jsonl" 2>/dev/null \
  | head -1 | xargs dirname)
mkdir -p /tmp/sessions-mytest
cp -r "$SESSION_DIR/." /tmp/sessions-mytest/

# 3. Assert on the predictable path
assert:
  recursive-session:
    input: /tmp/sessions-mytest
```

Any case that re-uses the setup session via a discovery command (`sessions list`,
`episodic_recall`, etc.) must also `unset RECURSIVE_SESSIONS_DIR` so it looks
under the same `RECURSIVE_HOME`. See `e2e/tests/11-session-resume.yaml` and
`e2e/tests/00-smoke.yaml` for the canonical form.

Always clean up in teardown:
```bash
rm -rf /tmp/rh-mytest /tmp/sessions-mytest
```

### aimock fixtures: use `turnIndex`, not fragile text matching

Multi-turn fixtures **must** use `turnIndex` + `hasToolResult`:
```json
[
  { "turnIndex": 0, "response": { "tool_calls": [{ "name": "Read", ... }] } },
  { "turnIndex": 1, "hasToolResult": true, "response": { "tool_calls": [{ "name": "Write", ... }] } },
  { "turnIndex": 2, "hasToolResult": true, "response": { "content": "Done." } }
]
```

### HTTP API calls inside the container

- Always include `-H 'Content-Type: application/json'`
- `POST /sessions` body: `{"system_prompt": "..."}` (can be empty `{}`)
- `POST /sessions/:id/messages` field is **`content`**, not `message`
- Node.js scripts: use `http://127.0.0.1:PORT`, never `http://localhost:PORT`
  (Node 18 `fetch` resolves `localhost` → `::1` IPv6; the server binds IPv4 only)

### What `recursive loop` cannot assert

`recursive loop` does **not** produce `transcript.jsonl`. Never use
`recursive-session:` assertions for loop-mode tests. Use `file:` assertions only.

### `argusAI save:` cannot capture exec stdout

ArgusAI cannot capture `exec:` stdout into variables via `save:`. Pass
runtime state between cases through temp files (`echo "$ID" > /tmp/my-sid`).

## Skills available in this project

- `/recursive-loop` — act as the loop orchestrator: read roadmap, pick goals,
  launch the Flowcast self-improve flow (`.dev/flows/self-improve.flow.js`
  via `.dev/scripts/launch-flow.sh`), handle results. Use this when the
  user wants Recursive to self-improve rather than you directly editing code.
  The legacy `.dev/scripts/self-improve.sh` is deprecated.

<!-- gitnexus:start -->
# GitNexus — Code Intelligence

This project is indexed by GitNexus as **Recursive** (10081 symbols, 23995 relationships, 300 execution flows). Use the GitNexus MCP tools to understand code, assess impact, and navigate safely.

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
