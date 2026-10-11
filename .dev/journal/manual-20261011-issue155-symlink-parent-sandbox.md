# Issue #155 — sandbox: `Write`/`Edit` escaped through a symlinked parent directory

- **Date**: 2026-10-11
- **Goal**: Issue #155 (P0, invariant #3). `tools::resolve_within_any` gated its
  symlink-aware check on `abs_joined.exists()`, so a path that did **not** exist
  yet only went through the lexical `starts_with` check and was returned
  uncanonicalised. Callers (`WriteFile`, `EditTool`) then called
  `create_dir_all(parent)` + write, and the OS followed any symlink in an
  ancestor directory. Measured on `origin/main`: with `docs -> <outside>`,
  `resolve_within(root, "docs/exists.txt")` was rejected (the target exists)
  while `resolve_within(root, "docs/new.txt")` succeeded — same link,
  already-existing file blocked, new file allowed.

## Fix

- `src/tools/dispatch.rs`
  - New `canonicalise_with_existing_ancestor(&Path) -> io::Result<PathBuf>`
    (thin wrapper over `resolve_links`): walks up to the longest **existing**
    ancestor, `canonicalize()`s it, then re-attaches the not-yet-existing tail.
    Works for files/directories that do not exist yet, which is exactly the
    case `canonicalize()` cannot handle.
  - **Review round 2, finding 1**: the walk's existence probe is
    `symlink_metadata`, **not** `Path::exists`. A symlink is a directory entry
    of its own even when its target is missing, while `exists()` follows the
    link and reports `false` — so a *dangling* leaf link used to be treated as
    "a component about to be created" and re-attached to the canonical
    ancestor unchanged, and `Write('notes')` then created the target outside
    the sandbox. Links found by the walk are now followed by `resolve_links`
    (relative targets rebased on the link's canonicalised parent; a 40-hop
    bound rejects cycles). Both `docs -> <outside>` + `docs/new.txt` and
    `notes -> <outside>/created_by_agent.txt` resolve out of the roots.
  - `resolve_within_any` now applies that resolution to **every** path (no
    `exists()` branch) and re-checks containment of the resolved form against
    each root's resolved form — so a root that is itself a symlink (`/tmp` →
    `/private/tmp`, a symlinked workspace, a macOS `TempDir` under
    `/var/folders`) is not falsely rejected.
  - **Review round 2, finding 2**: the root side goes through the *same*
    helper (`canonicalise_with_existing_ancestor(&abs_root).unwrap_or(abs_root)`,
    previously a bare `abs_root.canonicalize()`). A root that does not exist on
    disk yet used to be compared in lexical form against a candidate resolved
    through `/tmp -> /private/tmp`, which made
    `resolve_within("/tmp/not-yet-created", "a.txt")` fail with a misleading
    "resolves via symlink to a location outside all sandbox roots" — a
    regression for `--add-dir`/extra-root paths created later. Both sides now
    degrade identically.
  - The cheap lexical check is kept in front of it: it produces the stable
    `escapes sandbox roots` error for `../` / absolute escapes and is the only
    check possible when the root does not exist on disk.

- **Deliberate deviation from the issue's suggestion #2**: the function keeps
  returning `abs_joined` (the caller-shaped absolute path), not the resolved
  canonical path. Returning the canonical form changes the *path identity* the
  callers see — measured blast radius on macOS: 23 existing tests
  (`fs`/`edit`/`registry`/`count_lines` read-state keys and transport path
  assertions) plus a real production behaviour in `tools/present.rs`, where
  `Deliverables::describe` does `abs.strip_prefix(self.workspace)` and would
  start emitting absolute `/private/var/...` paths instead of workspace-relative
  ones. The transport contract (`src/tools/transport_layer/transport.rs`)
  likewise treats `resolve_within_any` output as tier-relative, so host-side
  canonicalisation of the returned value is not generally right. The security
  property does not depend on the returned spelling: a path is only returned
  after the resolved-form containment check passed, and that resolution
  follows *every* symlink on the path — including a dangling leaf
  (round 2, finding 1). TOCTOU narrowing past that (return resolved /
  `O_NOFOLLOW` / `openat` walk) is left as the follow-up the issue itself
  flags; it is a narrower window than the deterministic dangling-leaf hole,
  which is fixed here.

## Files touched

- `src/tools/dispatch.rs` — ancestor-canonicalisation helper + reworked
  `resolve_within_any`; doc comments on `resolve_within` / `resolve_within_any`
  updated (the old text promised callers were responsible for non-existent
  paths — no longer true).
- `tests/invariants/sandbox.rs` — the invariant-#3 tests for the hole.

## Tests added

- `src/tools/dispatch.rs`:
  - `symlink_parent_blocks_new_file` (the hole: `docs/new.txt` → Err, message
    mentions symlink),
  - `symlink_parent_blocks_existing_file` (was already rejected; no regression),
  - `symlink_parent_blocks_nested_creation` (`docs/.ssh/newdir/x` → Err, so no
    out-of-sandbox `create_dir_all`),
  - `legit_nested_path_still_allowed` (ordinary new nested file still Ok),
  - `workspace_root_itself_is_a_symlink` (`ws -> real`: both a new and an
    existing file inside still resolve — the "canonical root vs lexical
    candidate" trap).
- `src/tools/dispatch.rs` (round 2):
  - `dangling_symlink_leaf_is_rejected` (`notes -> <outside>/created.txt`,
    target absent: Err for read and write),
  - `dangling_symlink_leaf_with_relative_target_is_rejected`
    (`notes -> ../../escaped.txt`: rebasing on the link's directory),
  - `symlink_leaf_chains_and_cycles` (`hop1 -> hop2 -> <outside>/f` rejected;
    `cycle_a <-> cycle_b` rejected instead of looping),
  - `dangling_symlink_leaf_inside_root_is_allowed` (`notes -> sub/missing.txt`
    stays legal — the check is containment, not "leaf must not be a link"),
  - `uncreated_root_under_symlinked_parent_still_matches` (round-2 finding 2).
- `tests/invariants/sandbox.rs`:
  - `resolve_within_rejects_new_file_through_symlinked_parent`,
  - `resolve_within_rejects_existing_file_through_symlinked_parent`,
  - `resolve_within_rejects_dangling_symlink_leaf` (round 2),
  - `resolve_within_allows_uncreated_root_under_symlinked_parent` (round 2),
  - `writefile_rejects_symlinked_parent_dir` — end-to-end `Write` tool: the
    call errors **and** no file/directory appears outside the sandbox,
  - `writefile_rejects_dangling_symlink_leaf` / `edit_rejects_dangling_symlink_leaf`
    (round 2) — end-to-end `Write`/`Edit` through a dangling leaf link: the call
    errors **and** the out-of-sandbox target is never created.

## Notes

- `ReadFile`/`Glob`/`Search`/`count_lines`/`estimate_tokens` share the same
  primitive, so read paths are covered by the same change; writes still hit the
  `AccessTier::ReadWrite` gate (a new file under a read-only root is still
  rejected — `read_only_root_blocks_write` unchanged).
- Verified: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets
  --all-features -- -D warnings`, `cargo clippy --lib --no-default-features --
  -D warnings`, `cargo test --workspace --no-fail-fast` all green.
- No dependency changes; no new sandbox tier; `PROTECTED_PATHS` untouched.
