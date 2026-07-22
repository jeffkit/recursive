# Goal 329 — Promote `src/compact.rs` to `src/compact/` module directory

**Roadmap**: Compaction upgrade (Phase — foundation refactor)

**Design principle check**:
- Implemented as: `git mv src/compact.rs src/compact/mod.rs` (Rust treats
  `src/compact.rs` and `src/compact/mod.rs` as the same module path
  `crate::compact`).
- ❌ Does NOT branch inside `src/run_core.rs::RunCore::run_inner`.
- ❌ Does NOT change any public API, behavior, or test assertions — pure file
  relocation to establish a directory that later compaction goals (micro /
  reinject / retry / prompt submodules) can add files into.

## Why

The compaction upgrade plan adds several new concerns over the next ~13
goals: a no-LLM `Microcompactor`, a `PostCompactReinjector`, a PTL-retry
helper, and richer prompt templates. Putting all of them into the existing
single `src/compact.rs` would balloon it past 1000 lines and mix unrelated
concerns.

This goal establishes the `src/compact/` directory **without** splitting any
internals yet. Later goals add `micro.rs`, `reinject.rs`, `retry.rs`,
`prompt.rs` as `mod` declarations inside `compact/mod.rs`. Doing the move in
its own behavior-preserving goal means:
- The move is reviewed in isolation (no behavior diff to argue about).
- Later goals diff against a clean `compact/mod.rs` baseline instead of the
  old monolith.
- A regression here is trivially attributable (it can only be the move).

## Scope (do exactly this, no more)

### 1. Move the file

```bash
git mv src/compact.rs src/compact/mod.rs
```

That is the entire source change. Do **not** split `Compactor` into
submodules, do **not** extract `COMPACT_SCHEMA` / `render_structured` /
`try_structured_compact` / `safe_split_point` / `compact` / `apply_to_transcript`
into separate files — those stay verbatim in `compact/mod.rs`. Submodule
extraction happens in later goals, not this one.

### 2. Verify the module path is unchanged

`crate::compact::Compactor` (and every other `pub` item in the file) must
still resolve identically. Confirm by grepping that no call site needs
updating:

```bash
rg 'use crate::compact' src/ crates/
rg 'crate::compact::' src/ crates/
```

Both should return the same results as before the move (the module path
`crate::compact` is identical whether the file is `src/compact.rs` or
`src/compact/mod.rs`). If any import used a `compact.rs`-specific path that
breaks, fix it — but none are expected.

### 3. Tests

No new tests. The existing `#[cfg(test)] mod tests` block inside the moved
file must pass unchanged — that is the proof the move is behavior-preserving.
Do not add a test for "the move worked"; the existing suite is the test.

## Acceptance

- `cargo build --workspace` green
- `cargo test --workspace` green (same pass count as before the move)
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `git diff --stat` shows exactly one rename: `src/compact.rs` →
  `src/compact/mod.rs` (no content changes; `git diff` should be empty after
  rename detection)
- `rg 'use crate::compact|crate::compact::' src/ crates/` output unchanged

## Notes for the agent

- This is a `git mv`, not a delete + create — preserves history and ensures
  `git diff` detects the rename (empty content diff). Do not use
  `Write` to recreate the file; use the shell `git mv`.
- `src/compact/mod.rs` is a sibling of `src/compact.rs` only conceptually;
  after the move there is no `src/compact.rs` on disk. Do not leave both.
- The `tests/` integration tests (`tests/compact_boundary.rs`,
  `tests/invariants/tool_call_pairing.rs`, etc.) reference
  `crate::compact::Compactor` via `use recursive::...` re-exports — those
  must keep working untouched.
- **DO NOT modify** any file other than the rename. In particular do NOT
  touch `src/run_core.rs`, `src/runtime.rs`, `src/lib.rs`, `src/llm/`,
  `crates/`, or any test file. If `src/lib.rs` has a `mod compact;`
  declaration, it resolves to either `src/compact.rs` or `src/compact/mod.rs`
  automatically — no edit needed.
- After the move, write a journal entry at
  `.dev/journal/manual-<YYYYMMDD>-compact-mod-split.md` per `CLAUDE.md`,
  noting this is a behavior-preserving rename prerequisite for the
  compaction upgrade series (goals 329–342).
