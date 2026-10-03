# 20261003 — manual-20261003-issue80-knowledge-domain-split (2/3)

## Goal

Issue #80 (拆单 from #60, part 2/3, depends-on #81): 知识与检索域拆层 —
move the knowledge/retrieval-domain tools out of the flat `src/tools/`
directory into a new `src/knowledge/` module, with `src/tools/mod.rs`
keeping only re-exports (same compat strategy as 1/3).

## Files touched

Moved (`git mv`, 6 tool files from `src/tools/` → `src/knowledge/`):

- `facts.rs` (Memory Layer 2 — semantic facts, 1847 lines)
- `memory.rs` (Memory Layer 1 — working-memory scratchpad + legacy notes, 1392)
- `episodic_recall.rs` (Memory Layer 3 — session transcript search, 568)
- `estimate_tokens.rs` (347) / `count_lines.rs` (331) — read-only inspection
- `search.rs` (`Grep` tool, 756)

New:

- `src/knowledge/mod.rs` — submodule declarations + domain doc note.

Edited:

- `src/lib.rs` — `pub mod knowledge;` (alphabetical, between `kernel` and
  `llm`).
- `src/knowledge/{count_lines,search}.rs` — `use super::transport::…` →
  `use crate::tools::transport::…`; `super::{resolve_within_any, …}` →
  `crate::tools::{resolve_within_any, …}`; test-only
  `impl super::super::transport::ToolTransport` → `crate::tools::transport::…`.
  `facts.rs` / `memory.rs` / `episodic_recall.rs` / `estimate_tokens.rs`
  already used `crate::tools::…` absolute paths — untouched.
- `src/tools/mod.rs` — replaced the 6 `pub mod` declarations with
  `pub use crate::knowledge::<name>;` re-exports; all item-level `pub use`
  re-exports unchanged, so `crate::tools::facts`, `recursive::tools::SearchFiles`,
  … keep resolving for every existing caller (config.rs, cli/builder.rs,
  registry.rs, tests).
- `tests/invariants/test_coverage.rs` — MUST_HAVE_TESTS entries updated:
  `src/tools/{search,count_lines,estimate_tokens,facts,episodic_recall}.rs`
  → `src/knowledge/…`.
- docs (guarded by `tests/docs_living_paths.rs`): source-path references in
  `docs/architecture/tools/{index,search,memory-tools,episodic-tool,facts-tools}.md`,
  `docs/architecture/memory/layer{1,2,3}-*.md`,
  `docs/architecture/layer0-injection.md`,
  `docs/architecture/execution-environments.md`, `docs/llm-gateway-compat.md`
  updated to `src/knowledge/…`.

## Tests added

None needed — pure code motion, zero behavior change; all existing unit
tests moved with their files (`use super::*` keeps working because the
submodule items did not move relative to their parent module).

## Notes

- Invariant #9 intent preserved: one tool per file, each registered via
  `pub mod` in `src/knowledge/mod.rs`. The
  `tool_files_are_registered_in_mod_rs` invariant test only scans
  `src/tools/`, which no longer contains the moved files.
- `src/tools/mod.rs` diff is purely: 6 `pub mod` lines → 6 `pub use` lines
  (+ comment); item re-exports and the inline test module untouched — no
  behavior diff for any consumer (verified by the full workspace suite).
- External callers confirmed unbroken: `src/config.rs` (`tools::facts::
  facts_summary`, `tools::memory::memory_summary/scratchpad_summary`,
  `tools::episodic_recall::episodic_recall_summary` in prod + test paths),
  `crates/recursive-cli/src/cli/builder.rs` (item imports), `tests/`
  — all resolve through the module re-exports without edits.
- Gates: `cargo test --workspace` green (58 suites ok, 2481 lib tests incl.),
  `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  clean, `cargo fmt --all -- --check` clean.
- Part 3/3 of #60 will follow the same pattern for the remaining domain.
