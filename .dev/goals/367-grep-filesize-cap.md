# Goal 367 — Cap file size in the Grep tool's read (prevent OOM on large files)

**Roadmap**: Tools / robustness — OOM defense on plausible workspace contents

**Design principle check**:
- Implemented as: a `metadata().len()` guard before `read_to_string` in the Grep tool + a skip-with-reason.
- ❌ Does NOT touch the agent kernel, run loop, or invariants. The grep search logic is unchanged; only adds a size gate.
- No new deps.

## Why (the OOM, with evidence)

`src/tools/search.rs` (the Grep tool) walks the workspace with `WalkDir` and reads each file wholesale:
```rust
let Ok(contents) = std::fs::read_to_string(path) else { continue; };   // line ~174
```
There is **no file-size cap**. The `Read` tool caps output at 256 KiB (`src/tools/fs.rs:233`), `Edit` refuses files > 1 GiB (`edit.rs:38`) — but `Grep` has neither. A workspace containing a multi-GB file (a log, a data dump, a committed `node_modules` bundle, a `.sqlite`, a core dump) causes the agent to `read_to_string` the whole thing into a `String`, spiking RSS by the file size and likely getting OOM-killed. Only the binary-by-extension skip list (`:165`) guards a handful of extensions.

The walk also has no `max_depth` and relies on the default `follow_links(false)` — explicit is better for documenting symlink-loop safety.

## Scope (do exactly this, no more)

### 1. Add a size cap before `read_to_string` (`src/tools/search.rs`)

Define a constant near the top of the file:
```rust
/// Maximum file size Grep will read into memory. Larger files are skipped
/// (with a reason emitted in the result) to avoid OOM on logs / data dumps /
/// bundled artifacts. 1 MiB covers virtually all source files.
const MAX_GREP_FILE_BYTES: u64 = 1024 * 1024;
```
Before the `read_to_string` call, check the file size:
```rust
// Skip files that would OOM if read wholesale. Source files are well under
// 1 MiB; anything larger is a log/data/artifact that grep shouldn't slurp.
let Ok(meta) = std::fs::metadata(path) else { continue };
if meta.len() > MAX_GREP_FILE_BYTES {
    // Optionally collect these into a "skipped large files" list surfaced in
    // the result — see how the existing binary-extension skip reports. If the
    // existing code silently `continue`s on binary files, match that (silent
    // skip) for consistency; if it surfaces them, surface these too.
    continue;
}
let Ok(contents) = std::fs::read_to_string(path) else { continue };
```
Read the surrounding code first: see how the binary-extension skip (`:165`) reports its skips (silent `continue` vs recorded). Match that exact style for consistency.

### 2. Make `follow_links(false)` explicit on the WalkDir builder

Find the `WalkDir::new(&scope)` call (around `:154`) and add `.follow_links(false)` even though it's the default — this documents intent against symlink loops and guards if the default ever changes. One-line addition.

### 3. Test

In `src/tools/search.rs`'s test module, add:
- `grep_skips_files_larger_than_cap`: create a temp dir, write a file larger than `MAX_GREP_FILE_BYTES` (e.g. write `1 MiB + 1` of `a` bytes) containing a line that WOULD match the grep pattern, write a small file that also matches, run the Grep tool against the dir with a matching pattern, assert the small file's hit is returned and the large file's is NOT. This pins the size cap.

Read the existing Grep tests for the harness pattern (how they construct the tool, invoke it, and assert on results). Mirror it. Use `tempfile::tempdir()` (already a dev-dep).

## Files NOT to touch

- `src/run_core.rs`, `src/kernel.rs`, `src/runtime.rs`.
- The `Read`/`Edit` tools' own caps (`fs.rs:233`, `edit.rs:38`) — they're correct; don't unify them in this goal.
- The grep pattern-matching logic itself.
- `.dev/flows/`.

## Acceptance

- `cargo test -p recursive-agent search::tests::grep_skips` — the new test passes.
- `cargo test --workspace` green.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.
- Grep: `rg "read_to_string" src/tools/search.rs` shows the `metadata` guard immediately before it.

## Notes for the agent

- **Pick a sensible cap.** 1 MiB is the recommendation (source files are well under; logs/data are over). Don't make it configurable in this goal — a constant is fine.
- **Match the existing skip-reporting style.** If binary files are silently skipped, skip large files silently too. Don't invent a new reporting mechanism.
- **The test must write >cap bytes**, not just claim the file is large. Use a loop or `String::with_capacity` + `"a".repeat(...)`. Keep the pattern simple (e.g. search for "match" and put "match" in both files).
- **`follow_links(false)` is the WalkDir default** — adding it explicitly is documentation, not a behavior change. Don't add a `max_depth` (grep should recurse fully); only the symlink explicitness is in scope.
