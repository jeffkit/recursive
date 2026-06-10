# Goal 267 — Unified atomic_write helper (storage hardening)

**Roadmap**: Phase 17 (Production Hardening) — P0 from
`docs/review/architecture-review-2026-06-10.md` (NEW-STORE-2 + NEW-STORE-3)

**Design principle check**:
- Implemented as: new helper module `src/atomic.rs` + 5 call-site
  migrations + 3 duplicates removed
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- ❌ Does NOT add a new feature flag — this is a refactor of existing
  persistence paths

## Why

The architecture review (2026-06-10, `architecture-review-2026-06-10.md`)
found:

1. **3 independent `atomic_write` implementations** that have *diverged*:
   - `src/session.rs:476` — sync, calls `f.sync_all()`, uses
     `.tmp-{name}-{pid}`
   - `src/team.rs:237` — sync, calls `f.sync_all()`, uses
     `.tmp-team-{name}-{pid}`
   - `src/storage/local.rs:19` — async, **does NOT call `f.sync_all()`**,
     uses `.tmp-{name}-{pid}`

   The async version silently loses durability on power-loss. POSIX
   `rename(2)` is atomic, but the data it names may not be on disk.
   This is the single most important fix in the persistence layer.

2. **5 non-atomic write sites** that should use atomic_write but
   currently call `std::fs::write` directly:
   - `src/storage/local.rs:114-128` — `save_memory`
   - `src/cost.rs:137-144` — `write_cost_json`
   - `src/cost.rs:151-211` — `update_meta_with_cost` (also has
     read-modify-write race with `SessionWriter`)
   - `src/session.rs:74-81` — `SessionFile::write_to`
   - `src/transcript.rs:46-51` — `transcript::write_to`
   - `src/checkpoint.rs:390` — `restore_paths` (most user-visible:
     clobbers user's working file on crash)

This goal is the **highest leverage** P0: extracting one helper
unlocks 5 one-line call-site fixes, and removing 3 duplicates stops
future divergence.

## Scope (do exactly this, no more)

### 1. New module: `src/atomic.rs`

```rust
//! Atomic file write helper — single source of truth for
//! "write-then-rename" persistence across the recursive codebase.
//!
//! POSIX guarantees `rename(2)` is atomic. We additionally call
//! `f.sync_all()` and `dir.sync_all()` so the new file's data and
//! the directory entry are durable on power loss.
//!
//! Use this for every write to durable state: session meta, cost
//! json, transcripts, checkpoint restores, memory blobs, etc.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// Sync write: caller controls bytes. Used by JSONL streams and
/// in-memory assembled blobs.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    atomic_write_impl(path, bytes, false)
}

/// Async wrapper around `atomic_write` for async call sites.
/// Uses `tokio::task::spawn_blocking` under the hood so the blocking
/// fs work does not stall the async runtime.
pub async fn atomic_write_async(path: PathBuf, bytes: Vec<u8>) -> io::Result<()> {
    tokio::task::spawn_blocking(move || atomic_write(&path, &bytes))
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
}

fn atomic_write_impl(path: &Path, bytes: &[u8], is_async: bool) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = parent.join(format!(
        ".tmp-{}-{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file"),
        std::process::id()
    ));

    // Write to temp, fsync the data, then the parent dir, then rename.
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    // fsync parent dir so the rename is durable.
    if let Ok(dir) = File::open(parent) {
        let _ = dir.sync_all();
    }
    fs::rename(&tmp, path)?;
    Ok(())
}
```

Notes:
- Use **stable temp names** suffixed with PID only (no nanosecond
  timestamp) to match the existing convention.
- Do **not** include `is_async: bool` in the public surface — the
  `atomic_write_async` wrapper is the only async entry point and
  it calls the sync core via `spawn_blocking`.
- The `is_async` flag is internal only. Drop it from the public API.

### 2. Delete the 3 duplicate helpers

- `src/session.rs:476` — delete the local `atomic_write` function;
  replace its call sites with `crate::atomic::atomic_write`.
- `src/team.rs:237` — same.
- `src/storage/local.rs:19` — delete local helper; route through
  `crate::atomic::atomic_write_async` (so the async `save_transcript`
  keeps its async signature, but the actual fs work is sync under
  the hood, which is what we want).

### 3. Migrate 5 non-atomic write sites

- `src/storage/local.rs:114-128` — `save_memory` →
  `crate::atomic::atomic_write_async(path, value.into_bytes())`
- `src/cost.rs:137-144` — `write_cost_json` → `atomic_write`
- `src/cost.rs:151-211` — `update_meta_with_cost`:
  - **Read** the existing `.meta.json` *under the session writer
    mutex* (currently it does a bare read). Wire the
    `SessionWriter::bump_updated_at` lock to also gate this path,
    OR refactor `update_meta_with_cost` to take an `&SessionWriter`
    handle.
  - Once the read is gated, write through `atomic_write`.
- `src/session.rs:74-81` — `SessionFile::write_to` → `atomic_write`
- `src/transcript.rs:46-51` — `transcript::write_to` → `atomic_write`
- `src/checkpoint.rs:390` — `restore_paths` → `atomic_write(&abs,
  want.as_slice())`. This is the **most user-visible** fix: a crash
  mid-restore previously truncated the user's working file to half
  its content. Now it stays intact or fully restored.

### 4. Wire `atomic` into `src/lib.rs`

Add `pub mod atomic;` alongside the other top-level modules.

### 5. Tests (in `src/atomic.rs`, `#[cfg(test)] mod tests`)

- `test_atomic_write_creates_file` — write to a tempdir, assert
  file exists with correct bytes.
- `test_atomic_write_overwrites` — write twice, assert second wins.
- `test_atomic_write_cleans_tmp_on_success` — assert no `.tmp-*`
  file remains in the parent dir.
- `test_atomic_write_no_partial_content_on_interrupted` — simulate
  by killing the process via `std::process::exit(0)` from inside the
  write (impossible without a test seam) — *skip* this test, since
  the temp+rename pattern is what makes this property hold by
  construction.
- `test_atomic_write_async_roundtrip` — async wrapper returns the
  same bytes.
- `test_atomic_write_crash_simulation_via_oom` — optional: use a
  very large buffer and OOM, assert no partial file remains. Skip
  if too flaky on CI.
- `test_restore_paths_no_partial` — checkpoint test: restore a
  pre-existing file, assert that during the restore, the original
  file is still readable (test by reading the target path during
  the operation via a hook). This is the property users care about.

### 6. Property test (optional, `proptest` not currently a dep)

Skip — the unit tests above cover the contract. Adding proptest as
a new dep violates principle 4 ("no new deps without justification").

## Acceptance

- `cargo test --workspace` — green
- `cargo clippy --all-targets --all-features -- -D warnings` — clean
- `cargo fmt --all` — applied
- `git grep "std::fs::write\|tokio::fs::write" -- 'src/'` — should
  only show test sites (e.g. tests that intentionally exercise
  non-atomic writes for failure-mode tests). The 6 production sites
  listed above must all be gone.
- The 3 local `atomic_write` helpers in `session.rs`, `team.rs`,
  `storage/local.rs` are deleted (verify with
  `git grep "fn atomic_write" -- 'src/'` — should only match the
  canonical definition in `src/atomic.rs`).
- A new integration test under `tests/` that:
  1. Creates a session, writes a checkpoint, then `kill -9`s the
     process via a forked child (out of scope for unit tests).
     **Skip** — covered by manual smoke test in dev.
  2. Asserts that after a `restore_paths` operation, the target
     file is either fully old content OR fully new content (never
     partial). This is a unit-testable property using a custom
     reader that snapshots the file's size at intermediate points.

## Notes for the agent

- The `save_memory` site is async. Use the `atomic_write_async`
  wrapper. Do not block the async runtime on `std::fs::*`.
- The `update_meta_with_cost` race is a real bug separate from
  atomicity. You MUST take the `SessionWriter` mutex when reading
  the existing meta. If `SessionWriter` is not reachable from the
  call site, add a method `read_meta_under_lock` to `SessionWriter`
  and call it.
- Do not add the `is_async: bool` parameter to the public API.
- Do not change the temp-file naming convention (`.tmp-{name}-{pid}`).
  The convention is load-bearing for existing log analysis.
- The `sync_all()` on the parent dir is best-effort — Windows
  doesn't support it the same way; the `if let Ok(dir)` guard is
  intentional.
- **DO NOT modify** any of these files outside scope:
  - `src/agent/*` — kernel untouched
  - `src/llm/*` — providers untouched
  - `src/http/*` — interfaces untouched
  - `src/tools/*` — tool registry untouched
  - `src/tui/*` — UI untouched
  - `src/mcp_server.rs`, `src/hooks/*` — untouched
  - Any other file not listed in the Scope section above
- Estimated diff size: 1 new file (~80 lines) + 6 file edits
  (≤15 lines each) = ~150 net additions, ~30 deletions.
