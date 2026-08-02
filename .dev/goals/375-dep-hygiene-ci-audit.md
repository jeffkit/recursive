# Goal 375 — Add `cargo audit` + `cargo machete` to CI; narrow AWS TLS feature

**Roadmap**: Supply-chain hygiene — no advisory/unused-dep gate; AWS pulls legacy TLS cluster

**Design principle check**:
- Implemented as: (a) add a `cargo audit` + `cargo machete` step to `.github/workflows/ci.yml`;
  (b) narrow the `aws-config`/`aws-sdk-s3` features to `rustls` so the legacy
  `hyper 0.14 / h2 0.3 / rustls 0.21` cluster drops from `Cargo.lock`.
- ❌ Does NOT touch any `src/` production code. Only `Cargo.toml`, `Cargo.lock` (via
  `cargo update`), and `.github/workflows/ci.yml`. No new runtime deps.
- No new deps.

**This is a Cargo/CI-only goal** — no `src/` change → the e2e gate is skipped (diff-scope
short-circuit). Fast iteration.

## Why (the gaps, with evidence)

### Gap 1 — No advisory or unused-dep gate in CI

`.github/workflows/` has `ci.yml`, `docker-image.yml`, `docs.yml`, `release.yml`.
`grep -rn 'audit\|machete\|udeps' .github/` returns **0 hits** — there is no automated
check for RUSTSEC advisories or unused dependencies. A static sweep (2026-08-02) found no
known-unmaintained crates currently, but there is no gate to catch the next one. `cargo
audit` is the standard tool; `cargo machete` catches unused deps that bloat compile time.

### Gap 2 — AWS SDK pulls a legacy TLS/HTTP cluster into the lock

`Cargo.toml:105-106`:
```toml
aws-sdk-s3 = { version = "1", optional = true }
aws-config = { version = "1", features = ["behavior-version-latest"], optional = true }
```
Neither pins a TLS feature, so `aws-smithy-http-client` defaults to pulling **both** the
modern rustls line AND a legacy cluster: `hyper 0.14.32`, `h2 0.3.27`, `rustls 0.21.12`,
`hyper-rustls 0.24.2`, `tokio-rustls 0.24.1`, `rustls-webpki 0.101.7` (all in addition to
their modern `1.x`/`0.23`/`0.27` counterparts used by the rest of the workspace).

**Impact:** not compiled into the default binary (`cloud-runtime` is off by default), but
it bloats `Cargo.lock` (519 unique crates, 588 instances), `cargo metadata`, IDE indexing,
and any build that enables `cloud-runtime`. The legacy `rustls 0.21` predates the
default ring→aws-lc rotation and is the line most likely to attract a future advisory.
`native-tls`/`openssl`/`openssl-sys` are also in the lock for the same reason (feature
surface of transitive deps) — narrowing to `rustls` removes them too.

### (Informational, NOT in scope) Duplicate `axum` 0.7+0.8

`axum 0.7.9` is pulled solely by `tonic 0.12.3` (opentelemetry-otlp transitive, behind the
`recursive-cli` `otel` feature) while the workspace uses `axum 0.8`. This is a tonic
upstream issue — fixing requires bumping `opentelemetry-otlp`/`tonic` to a tonic 0.13+
release. **Don't fix here** — it's an upstream blocker, note as follow-up. The two
real-action items above are the scope.

## Scope (do exactly this, no more)

### 1. Add `cargo audit` + `cargo machete` steps to CI (`.github/workflows/ci.yml`)

Add a job (or extend an existing one) that:
- Installs `cargo-audit` and `cargo-machete` (via `cargo install` in the workflow, or a
  cached binary if the existing CI has a cache pattern — read `ci.yml` first).
- Runs `cargo audit` — on warning/error, fail the step. (Configure to fail on advisories;
  consider `--deny warnings` if you want RUSTSEC-unmaintained to also fail, but that may be
  noisy initially — start with `cargo audit` default behaviour and adjust.)
- Runs `cargo machete` — on unused deps found, fail (or warn initially; pick fail to make
  it a real gate, since the workspace is currently clean).
- Add the job to run on PRs + push to main (mirror the existing job's `on:` triggers).

Read `ci.yml` for the existing job structure (rust-toolchain, cache setup) and mirror it.
Don't introduce a new caching scheme if one exists.

### 2. Narrow AWS TLS features (`Cargo.toml`)

Change:
```toml
aws-sdk-s3 = { version = "1", optional = true }
aws-config = { version = "1", features = ["behavior-version-latest"], optional = true }
```
to:
```toml
aws-sdk-s3 = { version = "1", features = ["rustls"], optional = true }
aws-config = { version = "1", features = ["behavior-version-latest", "rustls"], optional = true }
```
(Verify the exact feature name — AWS SDK uses `rustls` as the feature; check
`aws-config`/`aws-sdk-s3` docs for the 1.x line. It may be `rustls` or
`behavior-version-latest` + `rustls`. If `rustls` isn't a valid feature name on 1.x, the
correct one is likely the `aws-smithy-http-client` feature — but the public feature on
`aws-config` is `rustls`. Confirm with `cargo metadata` or docs before committing.)

Then run `cargo update -p aws-config -p aws-sdk-s3` (or just `cargo update`) to refresh
the lock, and verify the legacy cluster drops:
```bash
# Before: hyper 0.14, h2 0.3, rustls 0.21, hyper-rustls 0.24 all present
# After (target): only the modern hyper 1.x / h2 0.4 / rustls 0.23+ remain
grep -E '^name = "(hyper|h2|rustls|hyper-rustls|tokio-rustls)"' Cargo.lock | sort -u
```
If the legacy versions persist, the feature name is wrong or AWS hasn't dropped them
upstream — investigate, don't force-remove from the lock by hand.

### 3. Verify the default build is unaffected

```bash
cargo build --workspace            # default features (cloud-runtime OFF)
cargo build --workspace --all-features   # cloud-runtime ON — must still compile
cargo test --workspace
```
The default build was never pulling the legacy cluster (feature off) — so it should be
identical. The `--all-features` build is where you'll see the change (and must still pass).

## Files NOT to touch

- `src/` — no production code changes. (If `cargo machete` flags an unused dep IN `src/`'s
  Cargo.toml, removing the dep declaration is fine, but that's Cargo.toml not src/.)
- Other crates' Cargo.toml features (don't touch reqwest, tokio, etc. — they're already
  well-managed: `default-features = false` + `rustls-tls` everywhere).
- The `axum 0.7`/`tonic` duplicate — upstream blocker, out of scope.
- `e2e/`, `.dev/flows/`.

## Acceptance

- `cargo build --workspace --all-features` green.
- `cargo test --workspace` green.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.
- Grep: `rg "cargo audit|cargo-audit" .github/workflows/ci.yml` returns ≥1 hit (the new CI step).
- Grep: `rg "aws-sdk-s3.*rustls|aws-config.*rustls" Cargo.toml` returns the narrowed feature.
- (Target, may not be fully achievable if AWS upstream hasn't dropped the legacy cluster:
  `grep '^name = "rustls"' Cargo.lock` shows fewer `0.21.x` lines than before. Document the
  before/after count in the journal.)
- Install + run `cargo audit` locally if possible to confirm it passes (0 advisories). If
  `cargo install cargo-audit` is too slow for the agent's budget, the CI addition is still
  the deliverable; note that local verification was skipped.

## Notes for the agent (traps)

- **Don't `cargo update` blindly.** Run it scoped (`-p aws-config -p aws-sdk-s3`) after
  the feature change, not a blanket `cargo update` (which could bump unrelated crates and
  balloon the diff). If a scoped update doesn't drop the legacy cluster, investigate why
  before widening.
- **The AWS feature name matters.** `aws-config` 1.x exposes a `rustls` feature (and a
  `native-tls` feature). Confirm via `cargo tree -f "{p} {f}" -p aws-config` or the SDK
  docs. If you set `rustls` and `cargo build --all-features` fails, the name is wrong —
  don't hack around it, look it up.
- **CI install of cargo-audit/machete is slow** (~1-2 min each compile). If the existing CI
  has a `Swatinem/rust-cache` or similar, cache the cargo bin dir. Read `ci.yml` for the
  pattern. Don't reinvent caching.
- **`cargo machete` may false-positive.** If it flags a dep that IS used (e.g. via a macro
  or re-export), add it to the machete ignore list rather than removing it. Verify each
  finding with `grep` before removing a dep declaration.
- **`cargo audit` exit code:** 0 = no advisories, non-zero = advisories found. The CI step
  fails on non-zero. If there ARE current advisories (the static sweep found none, but
  `cargo audit` is authoritative), the step will fail — that's the point. Either fix the
  advisory (if it's a simple version bump) or document why it's accepted (and `cargo audit
  ignore` it with a comment). Don't make the gate a no-op.
- **Default build must not change.** The legacy cluster is only pulled by `cloud-runtime`
  (off by default). After the feature change, `cargo build` (no flags) should produce an
  identical dependency graph to before. If it changes, something else regressed — investigate.
- **Cargo.lock is a deliverable.** The narrowed feature + `cargo update` produces a
  `Cargo.lock` diff (legacy cluster removed). Commit `Cargo.lock` alongside `Cargo.toml`.
  Don't `.gitignore` it or leave it dirty.
