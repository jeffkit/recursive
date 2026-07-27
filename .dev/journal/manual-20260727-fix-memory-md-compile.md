# Manual edit: fix-memory-md-compile

**Date**: 2026-07-27
**Goal**: Continue work from previous session (1c145a07-e89d-421f-88e7-b38400b8e1da) on memory system MD format migration

**Context**: The previous session hit context window limit while implementing the new `.md` memory format. The worktree `p2-1a-memory-md` had 8 compilation errors that needed fixing.

**Files touched**:
- `src/tools/memory.rs` (main work)
- `tests/integration.rs` (test updates)

**Fixes applied**:
1. **E0599** (line 434): Added proper error handling for `read_dir` entries
2. **E0308** (line 500): Added `.to_string()` for `default_index`
3. **E0277/E0271** (line 613): Fixed `Vec<&str>` to `Vec<String>` type mismatch
4. **E0382** (lines 826-835): Cloned `entry.content` before move to avoid borrow-after-move
5. **E0255**: Renamed new struct from `MemoryEntry` to `MemoryFileEntry` to avoid collision with imported `crate::memory::MemoryEntry`
6. **Missing Default**: Added `#[derive(Default)]` for `MemoryType`
7. **YAML parsing**: Fixed tag parsing to handle indentation (`strip_prefix("- ")` instead of `"  - "`)
8. **Backward compatibility**: Added fallback to legacy `memory.json` format in `Recall::execute`

**Test updates**:
- Updated `remember_recall_roundtrip_in_scripted_run` to check for new `.md` format instead of `memory.json`
- Updated assert message to accept both "saved memory entry" and "saved note" formats

**Quality gates**:
- ✅ `cargo test --workspace` - all 2080 tests pass
- ✅ `cargo clippy --all-targets --all-features -- -D warnings` - clean
- ✅ `cargo fmt --all` - formatted

**Notes**:
- The new memory format stores entries as `.md` files with YAML frontmatter in `<workspace>/.recursive/memory/`
- An index file `MEMORY.md` is maintained for quick overview
- Legacy `memory.json` format is still supported for backward compatibility
- The YAML parser is manual (no serde_yaml dependency) to keep dependencies minimal
- Tag parsing now handles arbitrary indentation via `trimmed.strip_prefix("- ")`
