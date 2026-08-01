# Manual edit: goal-357 — Release coherence: bump 0.8.0 → 0.8.1 + fix CHANGELOG commit counts

**Date**: 2026-08-02
**Goal**: Restore release-line coherence on `main` (172 commits past the `v0.8.0` tag
but still self-identifying as 0.8.0) and correct two drifted CHANGELOG commit counts.
Pure version/metadata/docs work; no Rust source, kernel, tools, or invariants touched.

## Why

`v0.8.0` points at `3f242f0 chore(release): prepare v0.8.0`, but `main` is **172**
commits ahead (recomputed: `git rev-list --count v0.8.0..HEAD` = 172 — the goal brief's
"169" was itself stale by 3). Every release-line manifest still claimed `0.8.0`, so a
`main` build ships a binary self-identifying as the tagged release while containing
ACP support, OTLP exporter, multi-sub-agent E2E, and the Goals 353/354/355 hardening.
Also `CHANGELOG.md` claimed "190 commits since 0.7.0" for the 0.8.0 section; the real
count is **192** (`git rev-list --count v0.7.0..v0.8.0`).

## SemVer decision: 0.8.1 (patch), NOT 0.9.0 — with one flagged caveat

Chose 0.8.1 per the goal's default: the 172 commits are dominated by bug fixes,
invariant hardening (Goals 353/354), a security dep swap (Goal 355), compaction
reliability work, and dev/E2E infra. **Caveat for the maintainer (do NOT ignore):**
I found concrete evidence of an additive public-API change in the range — `pub mod acp;`
was added to `src/lib.rs:20` unconditionally (NOT behind a feature flag), exposing
`AcpServer`, `AcpBridge`, `AcpSessionManager`, `SessionMetadata`, `PermissionOutcome`,
`ToolKind`, and the full ACP v1 request/response protocol surface. `v0.8.0` had no
`pub mod acp` (verified via `git show v0.8.0:src/lib.rs`). OTLP, by contrast, IS
feature-gated (`otel` in `crates/recursive-cli/Cargo.toml`). Per the goal's instruction
("if you do, stop and note it in the journal instead of guessing") I did NOT escalate
to 0.9.0 — the maintainer should decide before tagging `v0.8.1` whether to (a) accept
the patch bump, (b) cut 0.9.0, or (c) feature-gate `recursive::acp` first. Flagged in
the `v0.8.1` CHANGELOG "Note" line as tag-pending only; the SemVer caveat lives here.

## Files touched

- `Cargo.toml:15` — `version = "0.8.0"` → `"0.8.1"` (recursive-agent)
- `crates/recursive-cli/Cargo.toml:3` — `version = "0.8.0"` → `"0.8.1"`
- `crates/recursive-tui/Cargo.toml:3` — `version = "0.8.0"` → `"0.8.1"`
- `Cargo.lock` — updated by `cargo build --workspace`: the three release-line entries
  (`recursive-agent`, `recursive-cli`, `recursive-tui`) are now `0.8.1`.
- `CHANGELOG.md` — new `## 0.8.1` section at top (172 commits, highlights grouped under
  the same headings as 0.8.0: Features / Architecture & reliability / Bug Fixes /
  Dev & E2E); 0.8.0 section's "190 commits since 0.7.0" corrected to **192**. The
  historical `0.2.0 (unreleased)` section is untouched.

Not touched: `crates/agui-*/Cargo.toml`, `crates/tui-pty-harness/Cargo.toml` (0.1.0
line), any `src/` Rust source, `.dev/flows/`, `ROADMAP*.md`, `README.md`. **No git tag
created** — tag is a separate human step (noted in the CHANGELOG and here).

## Verification

- `cargo build --workspace` succeeds; all three release-line crates compile as `0.8.1`.
- Release-line lock entries: recursive-agent / recursive-cli / recursive-tui → `0.8.1`;
  agui-* / tui-pty-harness → `0.1.0` (unchanged). No release-line entry remains at
  `0.8.0`.
- **Acceptance-grep caveat:** the goal's literal greps `grep -c 'version = "0.8.0"'
  Cargo.lock` → `0` and `grep -c 'version = "0.8.1"' Cargo.lock` → `3` are unsatisfiable
  as written because unrelated third-party deps in the lock carry those versions
  (`base64-simd 0.8.0`, `bit-set 0.8.0`, `bit-vec 0.8.0`, `vsimd 0.8.0`; `compact_str
  0.8.1`, `moxcms 0.8.1`, `portable-pty 0.8.1`). The meaningful check — no workspace
  member at 0.8.0, exactly the three release-line members at 0.8.1 — passes (verified
  per-package above).
- `CHANGELOG.md` headline counts verified against git: 172 (`v0.8.0..HEAD`) and 192
  (`v0.7.0..v0.8.0`).
- `cargo fmt --all` clean, `cargo clippy --workspace --all-targets --all-features --
  -D warnings` clean, `cargo test --workspace` green.

## Notes

- ACP highlight line in the 0.8.1 CHANGELOG explicitly says "new `recursive::acp`
  module" — accurate and doubles as the public-API caveat in a user-visible place.
- The three hardening highlights required by the goal are each backed by their commit
  subjects: `5fdbead` (Goal 353, invariant #7), `62396c0` (Goal 354, invariant #5 —
  deny attr verified present in all 7 crates: root lib, recursive-cli main, recursive-
  tui lib, agui-protocol/client/tui libs, tui-pty-harness lib), `f7e9082` (Goal 355,
  RUSTSEC-2025-0068 → serde_yaml_ng).
