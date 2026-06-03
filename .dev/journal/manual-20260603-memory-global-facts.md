# Manual edit: memory-global-facts

**Date**: 2026-06-03
**Goal**: Fix facts_summary() to merge global + workspace facts so personal identity data stored in ~/.recursive/memory/facts.jsonl is included in the system prompt.
**Files touched**:
- src/tools/facts.rs — `facts_summary()` now loads both "global" and "workspace" scopes and merges active facts; test `test_m_facts_summary_empty` updated to pin HOME to a temp dir so real global facts don't leak into the assertion.
- src/tools/str_replace.rs — Fixed pre-existing clippy lint: collapsed consecutive `.replace()` calls into `replace([LEFT_SINGLE, RIGHT_SINGLE], "'")` form.
- src/tui/app/event_loop.rs — (already changed in prior session) spacing logic for scrollback flush.
- src/tui/ui/markdown.rs — (already changed in prior session) inline code in table cells fix.
**Tests added**: Updated `test_m_facts_summary_empty` to be hermetic (pins HOME).
**Notes**: The root cause was that `facts_summary()` called `load_facts(workspace, "workspace")` only. Global facts (scope="global") live at `~/.recursive/memory/facts.jsonl` and are loaded by computing `$HOME/.recursive/memory/facts.jsonl` in `load_facts()`. Personal user identity (e.g. user name, preferences) are typically stored as global facts, so they were silently dropped from the system prompt.
