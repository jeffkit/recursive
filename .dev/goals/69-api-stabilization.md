# Goal 69 — API Stabilization + Breaking Change Cleanup

**Roadmap**: Phase 7.1 — API stabilization

**Design principle check**:
- Implemented as: refactoring of public API surface. No new features.
- Does NOT add new capabilities to the agent loop.

## Why

The crate has grown organically through 68 goals. The public API has
accumulated experimental fields, inconsistent naming, and types that
shouldn't be public. Before publishing to crates.io, we need a clean,
minimal public surface that won't force breaking changes in 0.3.

## Scope (do exactly this, no more)

### 1. Audit `src/lib.rs` re-exports

Review every `pub use` in `src/lib.rs`. For each type, decide:
- **Keep public**: Core types users need (Agent, Config, Tool, LlmProvider, etc.)
- **Hide**: Internal types that leaked (implementation details)

Apply `#[doc(hidden)]` or remove from `pub use` for internal types.

### 2. Review struct field visibility

For each public struct, ensure:
- Fields users need to construct/read are `pub`
- Internal state fields are `pub(crate)` or private with accessor methods
- Builder patterns are preferred over direct field access for complex types

Key structs to review:
- `Config` — should all fields be public?
- `Skill` — too many fields exposed?
- `McpServerConfig` — internal?
- `TokenUsage` — all fields needed by users?

### 3. Naming consistency

Check for inconsistencies:
- Method naming: `new()` vs `create()` vs `build()`
- Error variants: consistent naming pattern
- Module organization: are things in the right module?

### 4. Deprecation markers

If renaming something, add `#[deprecated(since = "0.2.0", note = "use X instead")]`
rather than breaking existing code.

### 5. Tests

- Ensure all public API types/functions have at least a doc-test or unit test
- Add any missing tests for public methods

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo doc --no-deps` builds without warnings
- Public API surface is intentional, not accidental
- No regressions

## Notes for the agent

- Run `cargo doc --no-deps 2>&1` to see documentation warnings.
- Use `#[doc(hidden)]` for types that must remain `pub` for internal use
  but shouldn't appear in docs.
- Read `src/lib.rs` for the current public API surface.
- The goal is MINIMAL public API — less is more for a 0.2.0 release.
- Don't rename things without `#[deprecated]` or a very good reason.
- Focus on: what does a user importing `recursive` as a library need?
