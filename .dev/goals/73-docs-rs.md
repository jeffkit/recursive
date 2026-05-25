# Goal 73 — docs.rs Documentation

**Roadmap**: Phase 7.2 — docs.rs documentation (user-facing)

**Design principle check**:
- Implemented as: documentation comments only. No runtime changes.
- Does NOT modify any behavior.

## Why

Before publishing to crates.io, every public type and function needs
clear documentation. docs.rs auto-generates from `///` comments. Users
deciding whether to adopt the crate read these docs first.

## Scope (do exactly this, no more)

### 1. Crate-level documentation (`src/lib.rs`)

Add a comprehensive `//!` module doc at the top of `src/lib.rs`:
- One-paragraph description of what Recursive is
- Quick start example (3-5 lines showing basic usage)
- Feature flags explanation
- Link to examples/ directory

### 2. Document all public types

For every `pub` item re-exported from `src/lib.rs`, ensure it has:
- A `///` doc comment explaining what it is and when to use it
- At least one short code example for key types (Agent, Config, Tool trait)

Priority types to document:
- `Agent` + `AgentOutcome` + `FinishReason`
- `Config`
- `Tool` trait + `ToolRegistry`
- `LlmProvider` trait
- `Error` enum variants
- `Hook` trait + `HookRegistry`
- `Skill` struct

### 3. Module-level docs

Each `pub mod` should have a `//!` explaining what the module contains.

### 4. Verify with `cargo doc`

Run `cargo doc --no-deps --open` equivalent checks:
- No broken intra-doc links
- No missing docs warnings with `#![warn(missing_docs)]`

## Acceptance

- `cargo doc --no-deps 2>&1` produces no warnings
- `cargo test --doc` passes (doc examples compile)
- Every public type has at least a one-line description
- Key types have usage examples in their docs
- No runtime changes

## Notes for the agent

- Read `src/lib.rs` for the full list of public exports.
- Use `cargo doc --no-deps 2>&1 | grep warning` to find undocumented items.
- Doc examples must compile. Use `/// # ` prefix for hidden setup lines.
- Keep docs concise — one sentence for simple types, a paragraph + example
  for complex ones.
- Don't document internal/private items — focus only on `pub` API.
- Use `[`Type`]` syntax for intra-doc links between types.
