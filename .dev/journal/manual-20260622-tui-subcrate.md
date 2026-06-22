# Manual edit: tui-subcrate

**Date**: 2026-06-22
**Goal**: Extract TUI module as a standalone `recursive-tui` sub-crate (Goal 226)
**Files touched**:
- `Cargo.toml` — added `crates/recursive-tui` to workspace members (with `resolver = "2"`)
- `crates/recursive-tui/Cargo.toml` — new sub-crate manifest
- `crates/recursive-tui/src/lib.rs` — thin re-export wrapper
- `src/tools/install_skill.rs` — moved `SkillSearchResult`, `SkillZipFile`, `SkillSearchRequest`, `SkillFilesRequest`, `SkillInstallEvent` types here (from `tui/events.rs`) to break circular dependency
- `src/tui/events.rs` — replaced struct/enum definitions with `pub use` re-exports from `tools::install_skill`; added stub `SkillInstallEvent` for `not(feature = "skill-hub")` case

**Tests added**: none (existing tests still pass)

**Notes**:
- Cargo does not allow `recursive-agent` to depend on `recursive-tui` if `recursive-tui`
  also depends on `recursive` (the lib from `recursive-agent` package). This is a
  package-level circular dependency that Cargo rejects.
- Resolution: `recursive-tui` is a **thin re-export wrapper** crate. It depends on
  `recursive` (with `tui` feature), and re-exports `recursive::tui::*`. External users
  can depend on `recursive-tui` without pulling in the full `recursive-agent` package
  directly; the main binary (`src/main.rs`) continues using `recursive::tui::run()`
  unchanged.
- The long-term clean architecture would separate the CLI binary into its own workspace
  member (`crates/recursive-cli/`) so that it can depend on both `recursive` and
  `recursive-tui` without creating a package cycle. That is left as a future refactor.
- The `SkillInstall*` type migration to `tools/install_skill.rs` breaks the
  `install_skill.rs → tui::events` import path that would have been circular once TUI
  lived in its own crate.
