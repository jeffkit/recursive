# Goal 38 — Persistent Memory (cross-session note store)

**Roadmap**: 3.2 — Persistent Memory (Medium / S)

**Design principle check**:
- Implemented as: **new Tools** `remember` + `recall` +
  `forget` in `src/tools/memory.rs` + **system prompt source**
  (memory index injected at agent startup). No agent loop changes.
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop.

## Why

Today every agent session starts from zero. The agent re-learns the
project layout, re-discovers conventions, re-fails the same way.
Persistent memory lets the agent jot down lessons across sessions:

> "When testing reqwest-based code, always set explicit `.timeout()`
> + `.connect_timeout()` per AGENTS.md section 5."

Next session loads that note as part of its system prompt
preamble. Same shape as the `learn` skill in `.claude/skills/learn/`
but agent-native.

## Scope

Touches: new `src/tools/memory.rs`, `src/tools/mod.rs` (re-export),
`src/main.rs` (registration + system prompt injection).

### 1. Storage format

JSON file at `<workspace>/.recursive/memory.json` (or
`~/.recursive/memory.json` if `RECURSIVE_MEMORY_GLOBAL=1`). Schema:

```json
{
  "notes": [
    { "id": "N1", "tags": ["rust", "testing"], "text": "...", "ts": "2026-05-25T17:55:00Z" }
  ]
}
```

IDs auto-assign `N1`, `N2`, …, monotonically.

### 2. New tools

- **`remember`**: params `{ text: string, tags?: string[] }`.
  Appends to `notes`, returns the new ID.
- **`recall`**: params `{ query?: string, tag?: string, limit?: int=10 }`.
  Returns up to `limit` notes whose `text` or `tags` contain `query`
  (case-insensitive substring), or all tagged with `tag`. Format:
  ```
  N3 [rust,testing] When testing reqwest-based code, always set ...
  N7 [skill] Use apply_patch over write_file for ...
  ```
  No params → returns most-recent `limit` notes.
- **`forget`**: params `{ id: string }`. Removes the note. Returns
  "removed N3" or "no such id".

### 3. System prompt injection

At agent startup (in `src/main.rs`):

- If `memory.json` exists and is non-empty, build a summary block:
  ```
  # Memory (top 5 most recent notes; use `recall` for more)
  - N7 [skill] ...
  - N6 [rust] ...
  ...
  ```
- Prepend or append to the system prompt (your choice; appending
  after the default prompt is probably safer for ordering).

### 4. Tests in `src/tools/memory.rs`

- **Test A**: in a tmpdir, `remember "foo" tags=[a,b]` writes a
  JSON file; `recall query=foo` returns it.
- **Test B**: `remember` then `forget` then `recall` → empty.
- **Test C**: malformed JSON file errors cleanly (not panic).

## Acceptance

- `cargo build` green.
- `cargo test` green.
- `cargo clippy --all-targets -- -D warnings` green.
- `cargo fmt --all` clean.
- No memory file → agent behavior unchanged.

## Notes for the agent

- File locking: not needed for v1. Recursive doesn't yet support
  concurrent agent runs against the same workspace, and even if it
  did, last-writer-wins for a small JSON note store is acceptable.
- The 5-note injection cap is a starting point. Tunable later, but
  start small.
- Use `serde_json` (already a dep). No new crates.
- The `recall` substring search is intentionally dumb — no
  fuzzy / embedding / tf-idf. We can layer smarter retrieval later
  if usage suggests it.
- Coordinate with g35 / g36 / g37 — all touch `main.rs`. The
  memory injection block goes near the system-prompt building, not
  near tool registration; should not collide if everyone targets
  distinct edit zones.
- Use `apply_patch`. `.to_string()` over `.into()` in tests.
