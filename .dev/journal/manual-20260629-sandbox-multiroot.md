# Manual edit: sandbox multi-root + access tiers + runtime /add-dir

**Date**: 2026-06-29
**Goal**: Let the agent read (and selectively write) files outside its working
directory. Previously `Config::extra_dirs` (populated by `--add-dir`) was dead
config — `build_standard_tools` ignored it, and `resolve_within` only knew the
single workspace root. The TUI had no way to grant extra access at runtime.

## What changed

### L1 — multi-root sandbox (core)
- `src/tools/dispatch.rs`: new `AccessTier` enum (`ReadOnly`/`ReadWrite`) and
  `resolve_within_any(roots: &[(PathBuf, AccessTier)], path, write)` which
  lexical+canonicalises against any of the roots, enforcing the write tier.
  `resolve_within(root, path)` now delegates to it with a single `ReadWrite`
  root (backward compatible). Added `SharedSandboxRoots` alias
  (`Arc<RwLock<Vec<(PathBuf, AccessTier)>>>`) + `new_shared_sandbox_roots()`.
- `src/tools/{fs,edit,glob,search,count_lines,estimate_tokens}.rs`: every fs
  tool gained `extra_roots: Vec<(PathBuf, AccessTier)>` +
  `session_roots: Option<SharedSandboxRoots>` with `with_extra_roots` /
  `with_session_roots` / `with_session_roots_opt` builders and an `all_roots()`
  helper that merges workspace (rw) + extra + session snapshot. `execute`
  resolves via `resolve_within_any`. `glob`/`search` `relativise` reports extra
  roots as absolute paths so follow-up `Read` calls work.
- `src/tools/registry.rs`: new `build_standard_tools_with_roots(workspace,
  extra_roots, session_roots, skills, shell_timeout)`; `build_standard_tools`
  delegates with empty/None.
- `src/tools/mod.rs`, `src/lib.rs`: re-export the new primitives.

### L1 — config sources
- `src/config_file.rs`: new `[sandbox]` section with `extra_dirs` and
  `extra_readonly_dirs` (Vec<String>).
- `src/config.rs`: `Config.extra_readonly_dirs: Vec<PathBuf>`, resolved
  relative to cwd at load time alongside `extra_dirs`.
- `crates/recursive-cli/src/main.rs`: `--add-dir` now **extends** (not
  replaces) file-loaded `extra_dirs`.
- `crates/recursive-cli/src/cli/builder.rs`: collects extra roots
  (rw from `extra_dirs`, ro from `extra_readonly_dirs`) and feeds them to the
  fs tools' `with_extra_roots`; CLI path passes `None` for session_roots.
- `crates/recursive-tui/src/runtime_builder.rs`: `sandbox_extra_roots(config)`
  helper feeds the static extra roots into `build_standard_tools_with_roots`.

### L2 — read/write tiering
- `resolve_within_any(..., write=true)` rejects roots whose tier is
  `ReadOnly`. Write tools (`WriteFile`, `EditTool`) pass `write=true`; read
  tools pass `false`. Unit-tested in `dispatch.rs`.

### L3 — runtime interactive granting
- `build_runtime` / `build_runtime_for_tui` create a `SharedSandboxRoots`
  slot, pass `Some(slot.clone())` into the tools, and **return** the slot.
- `crates/recursive-tui/src/backend.rs`: `Backend` stores the slot
  (`session_roots` field) and threads it through both spawn paths.
- `crates/recursive-tui/src/app/mod.rs` + `state.rs`: `App.session_roots`
  field; `run_with_backend` syncs it from `backend.session_roots`.
- `crates/recursive-tui/src/commands.rs`: new `/add-dir <path> [--ro]`
  command (`:ro` suffix also supported). Canonicalises, checks it's a
  directory, de-dupes against existing roots, then appends to the shared slot.
  No-arg form lists currently-granted roots. Registered in `default_set`.

## Files touched
- src/tools/dispatch.rs, mod.rs, fs.rs, edit.rs, glob.rs, search.rs,
  count_lines.rs, estimate_tokens.rs, registry.rs
- src/config.rs, src/config_file.rs, src/lib.rs
- src/multi.rs, tests/v050_integration.rs, tests/http.rs, tests/agui_e2e.rs
  (Config literal updates for the new `extra_readonly_dirs` field)
- crates/recursive-cli/src/main.rs, cli/builder.rs
- crates/recursive-tui/src/runtime_builder.rs, backend.rs, lib.rs,
  app/mod.rs, app/state.rs, commands.rs

## Tests added
- `dispatch.rs`: `resolve_within_any` multi-root / tier / symlink / delegation.
- `fs.rs`: read from extra root, write blocked on ro extra root, write ok on rw.
- `commands.rs`: `/add-dir` registration, rw grant, `:ro` suffix, dedup,
  missing-path rejection; registry count bumped 15 → 16.

## Quality gates
`cargo test --workspace`, `cargo clippy --all-targets --all-features -- -D
warnings`, `cargo fmt --all` — all clean.

## Notes
- `runtime_builder.rs` carried pre-existing in-flight changes
  (`tui_system_prompt`, skill-index injection) that were not part of this
  edit; the `None` session_roots arg present in that in-flight version was
  replaced with `Some(session_roots.clone())`.
- The sandbox invariant (#3 in `.dev/AGENTS.md`) is preserved: every fs tool
  still goes through `resolve_within`/`resolve_within_any`; nothing bypasses
  containment. Extra roots only *widen* the allowed set, and only on an
  explicit user grant.
- No `DOC_CODE_MAP.md` exists, so the doc-sync rule was a no-op.
