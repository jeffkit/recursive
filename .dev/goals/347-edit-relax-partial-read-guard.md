# Goal 347 — Relax the Edit partial-read guard: verify on disk instead of forcing a full read

**Roadmap**: Tooling UX — stop the partial-read → forced-full-read context-bloat loop

**Design principle check**:
- Implemented as: a change to the guard block at the top of
  `EditTool::execute` (`src/tools/edit.rs:433-499`). When the file was only
  partially read, instead of hard-rejecting, fall through to the existing
  on-disk read + `old_string` match/unique-occurrence validation that the
  Edit tool already performs further down (`edit.rs:556-609`).
- ❌ Does NOT branch inside `src/run_core.rs::RunCore::run_inner`.
- ❌ Does NOT touch `WriteFile` — Write is a whole-file overwrite with no
  `old_string` anchor, so a partial read there genuinely means the model
  has not seen the whole file it is about to clobber. Its partial-read
  guard (`src/tools/fs.rs:466-484`) stays as a hard reject. Only Edit
  changes. Document this asymmetry in the journal.

## Why

Today, when the agent `Read`s a large file with `start_line`/`end_line`
(the correct thing to do — a full read of a 3500-line file like
`runtime.rs` bloats context badly) and then `Edit`s a snippet it saw in
that range, the Edit tool rejects with:

> `File \`{path}\` was only partially read (line range). Read the complete file before editing.`

That error message directly instructs the agent to read the *complete*
file, which for large files blows up the context window. Observed in the
real self-improve runs (goals 334/335/336/342): the agent enters a
`partial-read → Edit rejected → try full read → too big → partial-read`
loop until the watchdog kills it for no-growth. 3 of 9 compaction-upgrade
goals hit this and needed human rescue.

The Edit tool does not actually need the model to hold the whole file in
context to edit safely. It already:

1. Reads the current on-disk content itself (`edit.rs:557`, a private
   server-side read — this does NOT inflate the model's context).
2. Fuzzy-matches `old_string` against that disk content (`try_match`,
   `edit.rs:590`).
3. Rejects if `old_string` is absent or (without `replace_all`) occurs
   more than once (`edit.rs:596-609`).

So `old_string` is the model's proof it knows the exact bytes it is
replacing. If `old_string` is found and unique on disk, the edit is
well-defined regardless of whether the model read the whole file or a
slice. The partial-read guard is therefore redundant for Edit — the
`old_string` match is the real safety mechanism. (This is exactly how
fake-cc's `FileEditTool.validateInput` works: it reads the file from disk
and validates `findActualString` uniqueness; its "must have been read"
check is a secondary net, not the primary gate, and it never tells the
model to "read the complete file".)

The staleness check (file modified since last read) is still valuable,
but it can be satisfied on the partial-read path by comparing the disk
content around the `old_string` match — or simply re-read fresh from
disk (Edit already does this at `edit.rs:557`), so a stale cache entry
cannot cause a wrong edit: the disk read at line 557 is authoritative.

## Scope (do exactly this, no more)

### 1. `src/tools/edit.rs` — downgrade the partial-read branch

Today the match arm `Some(record) if record.is_partial =>` at line 457
hard-returns an error. Change it so a partial read does **not** reject.
Two acceptable implementations (pick whichever is cleaner with the
existing lock structure):

**Option A (preferred — minimal):** delete the `Some(record) if
record.is_partial =>` arm entirely so partial-read records fall through
to the `Some(record) =>` arm (the staleness check). The staleness check
compares cached content; for a partial read the cached `content` is
still the *full* file content (ReadFile caches the whole file even on a
ranged read — see `fs.rs:324-328`, it records `content.clone()` which is
the full file), so the staleness content-comparison still works. After
the guard block, execution proceeds to the on-disk read + `old_string`
match as today.

**Option B:** keep the arm but make it a no-op log + fall-through (do
not `return Err`). Ensure no `&mut`/await-inside-lock violation.

Either way, the end state: **a partial read no longer blocks an Edit.**
The `None =>` arm (file never read at all) STAYS a hard reject — editing
a file the model never laid eyes on is still disallowed; the model must
at least have `Read` it (full or partial) once.

### 2. Update the "never read" error is unchanged; remove the partial-read error

The `None =>` arm's message ("has not been read yet. Read it first
before editing.") stays verbatim. The partial-read-specific error string
("was only partially read (line range). Read the complete file before
editing.") is removed (the arm that produced it no longer exists).

### 3. `src/tools/fs.rs` — do NOT change WriteFile's guard

`WriteFile::execute` (`fs.rs:466-484`) has the same `is_partial` reject.
LEAVE IT. A whole-file overwrite has no `old_string` anchor, so a
partial read means the model has not seen the bytes it is about to
destroy. Add a one-line code comment at the WriteFile partial-read arm
pointing to this goal, explaining why Edit was relaxed but Write was not
("Edit verifies old_string against disk; Write has no anchor and would
clobber unseen content").

### 4. Tests (`src/tools/edit.rs` `#[cfg(test)] mod tests`)

Add (mirror the style of the existing read-state tests):

- `edit_succeeds_after_partial_read_when_old_string_found` — write a
  file, `Read` it with `start_line`/`end_line` (so `is_partial=true`),
  then `Edit` with an `old_string` that exists and is unique → edit
  succeeds, file content updated. This is the headline regression test:
  it would have failed before this goal and passes after.
- `edit_after_partial_read_still_rejects_missing_old_string` — partial
  read, then `Edit` with an `old_string` NOT in the file → still
  rejected with "String to replace not found" (proves we did not weaken
  the `old_string` validation, only the read guard).
- `edit_after_partial_read_still_rejects_ambiguous_old_string` — file
  with two identical lines, partial read, `Edit` with that line as
  `old_string` and `replace_all=false` → still rejected with the
  multiple-match message (proves uniqueness guard intact).
- `edit_still_rejects_never_read_file` — no `Read` at all → still
  rejected with "has not been read yet" (proves the `None` arm is
  unchanged).

Update any existing test that asserted the partial-read rejection for
*Edit* (if one exists) to assert success instead. Do NOT touch the
WriteFile partial-read test at `fs.rs:~1050` (`write_rejects_after_partial_read`)
— Write's behaviour is unchanged.

### 5. Journal

`.dev/journal/manual-<YYYYMMDD>-edit-relax-partial-read.md` — note:
- the real-world failure mode this fixes (the 334/335/336/342
  partial-read loops that needed rescue),
- the safety argument (`old_string` match is the real gate; Edit reads
  disk authoritatively at line 557),
- the deliberate Edit-vs-Write asymmetry.

## Acceptance

- `cargo test --workspace` green; clippy clean; fmt clean.
- The headline test `edit_succeeds_after_partial_read_when_old_string_found`
  passes (partial read + Edit with a found, unique old_string → success).
- `edit_still_rejects_never_read_file` still passes (never-read files
  remain uneditable).
- WriteFile's partial-read rejection is unchanged (its test still green).
- `tests/invariants/tool_call_pairing.rs` green (no behavioural break
  to pairing).
- No new `Error` variant; the relaxation removes a rejection path, it
  does not add error surface.

## Notes for the agent

- This is a SMALL, surgical change: delete one match arm (or make it
  fall through), keep the `None` arm, keep the staleness check, keep all
  the `old_string` validation below. Do not refactor the surrounding
  lock structure — the existing "extract under lock, await outside lock"
  pattern is correct and must be preserved (no `await` while holding the
  `MutexGuard`).
- The cached `ReadRecord.content` on a partial read is the FULL file
  content (ReadFile records the whole file regardless of the requested
  range — `fs.rs:324` passes `content.clone()` where `content` is the
  full file). So the staleness content-comparison in the `Some(record)
  =>` arm works correctly for partial reads too. Confirm this by reading
  `fs.rs:255-340` before changing the arm.
- Do NOT remove `is_partial` from `ReadRecord` — WriteFile still uses it,
  and the ReadFile tests assert on it.
- The `None =>` (never read) reject is a different invariant: "you must
  have looked at this file at least once this session." Keep it.
- Reference: `~/Downloads/fake-cc/src/tools/FileEditTool/FileEditTool.ts`
  (its `validateInput` reads disk + `findActualString` uniqueness; it
  never forces the model to read the complete file).
