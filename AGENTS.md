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
