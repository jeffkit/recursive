# Goal 348 — Decouple Edit staleness check from the "partial read caches full content" assumption

**Roadmap**: Tooling UX — harden the Goal-347 Edit relaxation against a future ReadFile change

**Design principle check**:
- Implemented as: a change to the staleness arm of `EditTool::execute`
  (`src/tools/edit.rs`, the `Some(record) =>` branch around line 470-501).
  When the record is a partial read, skip the cached-content equality check
  and rely on the on-disk read + `old_string` match below. Only full reads
  keep the content-equality staleness check.
- ❌ Does NOT branch inside `src/run_core.rs::RunCore::run_inner`.
- ❌ Does NOT change ReadFile / ReadFileState / ReadRecord.

## Why

Goal 347 relaxed the Edit partial-read guard: a partial read (via
`start_line`/`end_line`) no longer hard-rejects; it falls through to the
`Some(record) =>` arm, which runs the staleness check. That staleness check
compares `record.content` (the cached file content) against the freshly-read
disk content (`edit.rs:486-501`):

```rust
if let Some((_disk_mtime, cached_content)) = staleness_check {
    let disk_content = tokio::fs::read_to_string(&abs_path).await...;
    if disk_content != cached_content {
        return Err(... "modified since it was last read" ...);
    }
}
```

This currently works ONLY because `ReadFile` caches the *full* file content
even on a ranged read (`src/tools/fs.rs:324` records `content.clone()` where
`content` is the whole file). That coupling is a hidden landmine:

- If a future goal optimises `ReadFile` to cache only the requested line
  range (a plausible memory/parse-cost win for huge files), `record.content`
  on a partial read becomes just the slice. The staleness equality check
  `disk_content != cached_content` would then be `full_file != slice` →
  **always true** → every partial-read Edit would be rejected as "modified
  since read". Goal 347's relaxation would silently break, and the
  partial-read loop would return.

The Edit tool does not actually need the staleness cache check on the
partial-read path to be safe: further down, at `edit.rs:557`, it reads the
authoritative disk content itself and validates `old_string` is present and
unique. If the file was modified externally since the partial read, the
`old_string` the model copied from its (now-stale) view will very likely no
longer match → the existing "String to replace not found" / ambiguity
guards reject it. That is a precise, targeted rejection pointing at exactly
the region the model cares about — far better than a blanket "modified since
read" that offers no clue what changed.

So: the staleness cached-content check adds real value ONLY for full reads
(where the cache holds the whole file, so `disk == cache` is a meaningful
"nothing changed" signal and `disk != cache` is a meaningful "something
changed"). For partial reads it is both fragile (assumes full-content
caching) and redundant (old_string match already covers it). Decouple them.

## Scope (do exactly this, no more)

### 1. `src/tools/edit.rs` — gate the staleness content-check on full reads

In the `Some(record) =>` arm, the mtime check (`disk_mtime > record.timestamp`)
decides whether to run the content-fallback. Today it always extracts
`record.content` when mtime advanced. Change it so:

- **Full read** (`!record.is_partial`): behaviour unchanged — extract
  `(disk_mtime, record.content)` and run the content-equality check. The
  cache holds the whole file, so this is accurate.
- **Partial read** (`record.is_partial`): do NOT run the cached-content
  equality check. Skip straight through (no staleness rejection) — the
  on-disk read + `old_string` match below is the safety net. You can
  implement this by making the extracted staleness tuple `None` for partial
  records, e.g.:

```rust
Some(record) => {
    let disk_mtime = get_file_mtime(&abs_path);
    if disk_mtime > record.timestamp && !record.is_partial {
        // Full read: cached content is the whole file, so the
        // content-equality check is meaningful. Partial reads skip it
        // (their cache is a view, not the whole file; the old_string
        // match below already guards against external modification).
        Some((disk_mtime, record.content.clone()))
    } else {
        None
    }
}
```

(Exact shape may differ — the key invariant is: **the
`disk_content != cached_content` comparison is only reached for
`!record.is_partial`.** A partial read whose mtime advanced falls through
without a staleness rejection.)

### 2. Update the Goal-347 comment block above the arm

The comment at `edit.rs:433-451` explains the partial-read relaxation. Add a
short note that partial reads also skip the staleness content-check (and
why: old_string match covers it; avoids coupling to full-content caching).

### 3. Tests (`src/tools/edit.rs` `#[cfg(test)] mod tests`)

Add (mirror the existing Goal-347 partial-read tests' style; use
`make_slot()` + manual `record(...)` like the existing staleness tests):

- `edit_partial_read_skips_staleness_cache_check_when_mtime_advanced` —
  seed a file, record a **partial** read (`is_partial=true`) with an **old**
  timestamp, bump the file's mtime (write new content that STILL contains
  the `old_string`), then Edit with that `old_string` → **succeeds** (not
  rejected as "modified since read"). This is the headline test: it proves
  partial reads no longer hit the content-equality check. Before this goal,
  if ReadFile ever cached only a slice, this would have falsely rejected.
- `edit_full_read_still_runs_staleness_check_when_mtime_advanced` — same
  setup but record a **full** read (`is_partial=false`), bump mtime AND
  change content so `old_string` is gone → Edit rejected with "modified
  since it was last read" (proves full-read staleness is unchanged). NOTE:
  pick `old_string`/new content so the change is detected by staleness
  BEFORE the old_string match — i.e. the new disk content should NOT
  contain the old_string, but assert the error is the "modified since read"
  one, not "not found" (the staleness check runs first at line 486, before
  the old_string match at 557+).
- `edit_partial_read_still_rejects_when_old_string_gone_after_external_change`
  — partial read, bump mtime, change content so `old_string` is gone →
  Edit rejected with "String to replace not found" (proves the old_string
  match is the safety net for partial reads when the file really changed;
  it is NOT a blanket "modified since read").

Do NOT change existing staleness tests for full reads — they must stay green.

### 4. Journal

`.dev/journal/manual-<YYYYMMDD>-edit-staleness-decouple.md` — note:
- the hidden coupling (partial read currently works only because ReadFile
  caches the whole file),
- why old_string match is a sufficient safety net on the partial path,
- that this unlocks a future ReadFile optimisation (cache only the read
  range) without breaking Edit.

## Acceptance

- `cargo test --workspace` green; clippy clean (`-D warnings`); fmt clean.
- The headline test
  `edit_partial_read_skips_staleness_cache_check_when_mtime_advanced` passes.
- Existing full-read staleness tests unchanged and green.
- No change to ReadFile / ReadFileState / ReadRecord.
- `tests/invariants/tool_call_pairing.rs` green.

## Notes for the agent

- This is SMALL: one `&& !record.is_partial` (or equivalent branch) in the
  staleness arm, plus the comment + 3 tests. Do not refactor the lock
  structure (extract under lock, await outside).
- `record.is_partial` is a public field on `ReadRecord` — no new accessor
  needed.
- The hardest part is the tests' mtime manipulation: `get_file_mtime` reads
  epoch millis; to "bump" mtime, write the file again (which advances mtime)
  and/or sleep. Look at how existing staleness tests simulate an old record
  (they call `slot.lock().unwrap().record(path, is_partial, content,
  get_file_mtime(&path))` then re-write the file). Reuse that pattern.
- Reference: the Goal-347 comment block and tests in the same file.
