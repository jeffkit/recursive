# Issue #44 manual journal — publishable-crate version lockstep

**Date:** 2026-09-28
**Goal:** Fix version drift (#44): align all six publishable crates at 0.8.3 and pin internal path-dependency version reqs exactly so drift fails loudly.

## Files touched
- `Cargo.toml`, `crates/*/Cargo.toml` — bump agui-client / agui-protocol / agui-tui / recursive-cli / recursive-tui 0.8.2 → 0.8.3; internal deps pinned `=0.8.3`.
- `Cargo.lock` — regenerated.
- `tests/lockstep.rs` — new integration test asserting lockstep versions.
- `.dev/scripts/check-lockstep.sh` — new check script (accepts optional `vX.Y.Z` tag arg).
- `CHANGELOG.md` — Unreleased entry added.

## Notes
- Follow-up (not in scope): wire `check-lockstep.sh` into `release.yml`.
- Documentation-only touch-up per repo convention; no source changes beyond the existing uncommitted working tree.
