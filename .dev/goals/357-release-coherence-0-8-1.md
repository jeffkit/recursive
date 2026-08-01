# Goal 357 — Release coherence: bump 0.8.0 → 0.8.1 + fix CHANGELOG commit counts

**Roadmap**: Release coherence (post-0.8.0 main has drifted 169 commits past the tag)

**Design principle check**:
- Implemented as: version-string bumps in 3 `Cargo.toml`s + `Cargo.lock` + a new
  CHANGELOG section. Pure metadata/docs.
- ❌ Does NOT touch any Rust source logic, the kernel, tools, providers, or any
  invariant. No behaviour change.
- No new deps, no new tools.

## Why (the incoherence, with evidence)

The `v0.8.0` git tag points at `3f242f0 chore(release): prepare v0.8.0`, but `main`/HEAD
is **169 commits ahead** of that tag (`git rev-list --count v0.8.0..HEAD` = 169, verified).
Every workspace member on the 0.8.x release line still claims the already-tagged 0.8.0
version:

- `Cargo.toml:15` — `version = "0.8.0"`
- `crates/recursive-cli/Cargo.toml:3` — `version = "0.8.0"`
- `crates/recursive-tui/Cargo.toml:3` — `version = "0.8.0"`

(The `agui-*` and `tui-pty-harness` crates are on a separate `0.1.0` line and are NOT
touched by this goal — they predate the 0.8 line and version independently.)

Consequence: anyone building from `main` ships a binary self-identifying as the tagged
0.8.0 release while containing 169 unreleased commits (ACP support, OTLP exporter,
multi-sub-agent E2E, plus this hardening batch: Goals 353/354/355). This defeats SemVer
auditing and reproducibility.

Additionally, the CHANGELOG headline metric is wrong:
- `CHANGELOG.md:5` — `"190 commits since 0.7.0"`. Actual `git rev-list --count v0.7.0..v0.8.0`
  = **192** (the tag's own release-prep commit +1 more landed after the count was written).
  Recompute and correct.

## Scope (do exactly this, no more)

### 1. Bump version 0.8.0 → 0.8.1 in the 3 release-line manifests

**Why 0.8.1 (patch), not 0.9.0 (minor):** the 169 unreleased commits are dominated by
bug fixes, invariant hardening (Goals 353/354), a security dep swap (Goal 355), and
internal infra (E2E, flow, OTTL). No new stable public API has been added that a consumer
would depend on; the few feature additions (ACP, OTLP) are behind feature flags / internal.
Per SemVer, bugfix + hardening + no-new-public-API = patch bump. Do NOT bump to 0.9.0
unless you find concrete evidence of an additive public-API change in those 169 commits
(if you do, stop and note it in the journal instead of guessing).

Files to bump (set `version = "0.8.1"`):
- `Cargo.toml:15`
- `crates/recursive-cli/Cargo.toml:3`
- `crates/recursive-tui/Cargo.toml:3`

Then run `cargo build --workspace` so `Cargo.lock` updates the three `0.8.0` entries to
`0.8.1`. Verify with `grep -c 'version = "0.8.0"' Cargo.lock` → should be `0` after the
build. (The `0.1.0` crates are unchanged.)

### 2. Add a new `## 0.8.1` section at the TOP of `CHANGELOG.md`

Insert a new section above the existing `## 0.8.0` line. Use the real commit count —
**recompute it yourself**: `git rev-list --count v0.8.0..HEAD`. Derive the highlights from
the actual commits, NOT from guessing. Suggested workflow:

```bash
git log --oneline v0.8.0..HEAD                         # scan for themes
git log v0.8.0..HEAD --format='- %s' | head -60        # commit subjects to group
```

Group the highlights under the same headings the 0.8.0 section uses
(`### Features`, `### Architecture & reliability`, `### Bug Fixes`, `### Dev & E2E`).
Keep entries to one line each. The section MUST include (these are this hardening batch,
verify each by commit subject):

- **Bug Fixes**: mid-stream `Error::Cancelled` now persists the transcript (Invariant #7,
  Goal 353).
- **Architecture & reliability**: `#![deny(clippy::unwrap_used, expect_used)]` rolled out
  workspace-wide across all 7 crates (Invariant #5, Goal 354) — the shipping `recursive`
  binary no longer carries production `.unwrap()`/`.expect()` in session-resume / control
  paths.
- **Bug Fixes / Security**: replaced unsound unmaintained `serde_yml 0.0.12`
  (RUSTSEC-2025-0068) with `serde_yaml_ng` (Goal 355).
- Any other significant themes you find in the 169 commits (ACP, OTLP exporter,
  multi-sub-agent E2E, flow fixes) — list them under the right heading. If you're unsure
  whether something is user-facing, omit it rather than over-claim.

Open the section with: `<N> commits since 0.8.0. Highlights:` where `<N>` is the real
`git rev-list --count v0.8.0..HEAD` number at the time you write it.

### 3. Correct the 0.8.0 section's commit count

- `CHANGELOG.md:5` — change `"190 commits since 0.7.0"` to the real number
  `git rev-list --count v0.7.0..v0.8.0` (expected ~192, but recompute — do not hardcode).
  Keep the rest of the 0.8.0 section unchanged.

### 4. (Do NOT tag.) This goal only prepares main for a release; it does not create the
`v0.8.1` git tag. Tagging is a separate human step (the maintainer decides when to cut).
Leave a one-line note in the 0.8.1 CHANGELOG section or journal that the tag is pending.

## Files NOT to touch

- `crates/agui-*/Cargo.toml`, `crates/tui-pty-harness/Cargo.toml` — separate 0.1.0 line.
- Any `src/` Rust source — no behaviour change.
- `.dev/flows/`, `ROADMAP*.md` (roadmap drift is a separate P1 cleanup, not this goal).
- Do NOT create a git tag. Do NOT edit `README.md`'s version mentions (a separate P1).

## Acceptance

- `cargo build --workspace` succeeds with all three release-line crates at `0.8.1`.
- `grep -c 'version = "0.8.0"' Cargo.lock` → `0`.
- `grep -c 'version = "0.8.1"' Cargo.lock` → `3` (recursive-agent lib, recursive-cli,
  recursive-tui).
- `CHANGELOG.md` has a new `## 0.8.1` section at the top with a real commit count
  (verified by `git rev-list --count v0.8.0..HEAD`) and the three hardening highlights
  above.
- The 0.8.0 section's "commits since 0.7.0" number is corrected to the real
  `git rev-list --count v0.7.0..v0.8.0`.
- `cargo test --workspace` green, `cargo clippy --workspace --all-targets --all-features
  -- -D warnings` clean, `cargo fmt --all` clean. (A version bump should not break
  anything; these gates confirm the lock file is consistent.)

## Notes for the agent (traps)

- **0.8.1 not 0.9.0.** The default is a patch bump. Only escalate to 0.9.0 if you find a
  concrete additive public-API change (a new `pub fn`/`pub struct` on the stable surface,
  not behind a feature flag). When in doubt, patch. Document the reasoning in the journal.
- **Recompute counts from git, don't trust the existing text.** The whole point of this
  goal is that the existing CHANGELOG numbers drifted. Run `git rev-list --count
  v0.8.0..HEAD` and `git rev-list --count v0.7.0..v0.8.0` yourself and use those exact
  numbers.
- **`Cargo.lock` must reflect the bump.** After editing the 3 `Cargo.toml`s, run
  `cargo build --workspace` (or `cargo update -p recursive-agent --precise 0.8.1` etc.)
  so the lock file's three `0.8.0` entries become `0.8.1`. A partial lock update will
  fail the workspace build. Commit the lock alongside the manifests.
- **Don't summarize commits you don't understand.** If a commit subject is opaque, either
  read its diff briefly or omit it from the highlights. The CHANGELOG should be accurate,
  not exhaustive — a wrong claim is worse than a missing line.
- **The `0.2.0 (unreleased)` section** further down the CHANGELOG is a historical artifact;
  do NOT remove or touch it in this goal.
- **Keep the highlights to the 0.8.1 section's actual content.** Do NOT retroactively
  rewrite the 0.8.0 section's feature list — only fix its commit-count line.
