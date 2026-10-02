# Journal — manual-20261001-issue59-docs-drift

- **Date**: 2026-10-01
- **Goal**: #59 docs(architecture): 活文档指向已不存在的路径
- **Files touched**:
  - `docs/architecture/overview.md` — component map: `src/cli/` → `crates/recursive-cli/`, `src/tui/` → `crates/recursive-tui/`
  - `docs/architecture/agent-loop.md` — `Compactor` location `src/compact.rs` → `src/compact/`
  - `docs/architecture/invariants.md` — invariant #8 key files `src/compact.rs` → `src/compact/`
  - `docs/architecture/memory/layer3-episodic.md` — same `src/compact.rs` → `src/compact/`
  - `docs/architecture/layer0-injection.md` — entry-point paths → `crates/recursive-cli/src/...`, `crates/recursive-tui/src/...`
  - `docs/llm-gateway-compat.md` — dropped `:line` suffixes; paths verified
  - `docs/tui-acceptance-checklist.md` — ~130 `src/tui/...:line` refs → file-level `crates/recursive-tui/src/...` refs (option b: no line numbers in living docs)
  - `docs/tui-fake-cc-gap.md` — ~79 refs, same treatment
  - `.dev/AGENTS.md` — layout table: `main.rs / crates/recursive-cli` → `crates/recursive-cli`; `compact.rs` → `compact/` dir; `mod.rs ToolRegistry` → `registry.rs`; invariant #7 pointer → `crates/recursive-cli/src/cli/output.rs::exit_for_finish`
- **Tests added**: `tests/docs_living_paths.rs` — scans living docs (README, AGENTS.md, CLAUDE.md, .dev/AGENTS.md, docs/architecture/**, docs/*.md except review/exec-plans snapshots) for `src/|crates/|tests/|e2e/|.dev/scripts/` path references ending in `.rs` or `/src`, asserts each exists. `.dev/journal/**` explicitly exempt (frozen history — drift there is correct). Tolerates `::item` / `:line` suffixes; `src/agent.rs` / `src/permissions.rs` allowed as intentional historical mentions (legacy-split note, frozen session-log quote in layer3-episodic.md).
- **Notes**:
  - Landed as two WIP commits (`2e0d662`, `c54f96b`) from an earlier interrupted pipeline run; this session verified the work is complete and correct on top of `main`, no further source edits needed.
  - Verify commands from the issue: `grep -rn "src/compact\.rs" docs/architecture/ .dev/AGENTS.md` → empty; `grep -c "src/tui/" docs/tui-acceptance-checklist.md` → 0; `grep -rn "fn exit_for_finish" src/ crates/` → only `crates/recursive-cli/src/cli/output.rs:122`.
  - `.dev/journal/**` untouched (checked via `git log -- <path>` and diff stats).
