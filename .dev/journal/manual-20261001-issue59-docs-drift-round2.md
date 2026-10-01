# Journal — manual-20261001-issue59-docs-drift-round2

- **Date**: 2026-10-01
- **Goal**: #59 docs(architecture): 活文档指向已不存在的路径 — residual sweep after
  verifying the earlier interrupted pipeline work (commits `2e0d662` + `c54f96b`)
  actually landed and is correct
- **Files touched**:
  - `.dev/AGENTS.md` — layout table `tools/mod.rs` row: was "Tool trait + path
    sandboxing", but `Tool` / `ToolRegistry` live in `src/tools/registry.rs:52,157`
    and sandboxing in `src/tools/dispatch.rs::resolve_within`; `mod.rs` only
    re-exports (issue §③ row 2)
  - `docs/tui-acceptance-checklist.md` — last 2 bare `mod.rs:331-411` /
    `chat.rs:125-129` refs → full `crates/recursive-tui/src/...` file-level
    paths (issue option b: no `:line` in living docs; verified targets:
    exit path is `app/event_loop.rs`, plan-banner priority is `ui/chat.rs:129-136`)
  - `docs/tui-fake-cc-gap.md` — last 2 refs: `ui/modal.rs:43-71` (split across a
    line break, so the earlier sweep missed it) → file-level; `src/runtime.rs:204`
    → `src/runtime.rs::set_permission_hook` (fn exists at runtime.rs:981)
  - `.dev/OPERATIONS.md` — **newly covered living doc** (was outside the
    issue's file list but had the same disease): `src/main.rs` →
    `crates/recursive-cli/src/main.rs` (×4), `src/agent.rs` main-loop pointer →
    `src/run_core.rs::RunCore::run_inner`, parallelism table `src/agent.rs` →
    `src/agent/types.rs`
  - `tests/docs_living_paths.rs` — added `.dev/OPERATIONS.md` to `SCAN_FILES`
- **Tests added**: none new; extended `living_docs_reference_existing_paths`
  scope. **Negative test performed**: planted `src/tui/app.rs:160` into
  `docs/INTERNALS.md`, guard FAILED as expected; restored, guard green again.
- **Verification**:
  - `grep -rn "src/compact\.rs" docs/architecture/ .dev/AGENTS.md` → empty
  - `grep -c "src/tui/" docs/tui-acceptance-checklist.md` → 0
  - `grep -c "\.rs:[0-9]" docs/tui-acceptance-checklist.md docs/tui-fake-cc-gap.md` → 0 / 0
  - `fn exit_for_finish` → only `crates/recursive-cli/src/cli/output.rs:122`
  - guard test binary: ok (1 passed); `cargo clippy --test docs_living_paths -- -D warnings` clean
  - `git status --short .dev/journal/` → empty (history untouched)
- **Notes**:
  - The three WIP commits on this branch were produced by interrupted
    pipeline runs; this session audited each hunk against the real tree
    (every replaced path spot-checked to exist, e.g. `event_loop.rs`,
    `chat.rs:133`, `modal.rs` stack docs at line 1-4) rather than trusting
    the commit messages.
  - fake-cc references in `docs/tui-fake-cc-gap.md` (`src/components/...`,
    `src/vim/`, `src/bridge/` …) intentionally NOT flagged: those are the
    external reference project's paths, prefixed with `~/Downloads/fake-cc`
    context at line 19; the guard's boundary check already skips the
    `crates/…src/…` inner matches, and fake-cc paths under a heading saying
    "Reference TUI" are prose, not repo paths. Guard remains green because
    extraction only flags `.rs`-suffixed repo-rooted paths — `src/screens/REPL.tsx`
    etc. don't end in `.rs`. No exclusion hack needed.
  - Remaining accepted allowances in the guard: `src/agent.rs`,
    `src/permissions.rs` (historical mentions — legacy-split note in root
    AGENTS.md, frozen session-log quote in layer3-episodic.md).
