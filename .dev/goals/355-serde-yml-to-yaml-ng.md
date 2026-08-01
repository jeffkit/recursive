# Goal 355 — Replace unsound `serde_yml 0.0.12` with `serde_yaml_ng` (RUSTSEC-2025-0068)

**Roadmap**: Production hardening — dependency hygiene (RUSTSEC unsound dependency)

**Design principle check**:
- Implemented as: a dependency swap in `Cargo.toml` + a 2-site path-prefix rename
  (`serde_yml::` → `serde_yaml_ng::`) in one file.
- ❌ Does NOT modify the agent kernel, run loop, tools logic, or any invariant.
- The deserialized types (`AgentDefinitionRaw`) and all calling code stay identical —
  `serde_yaml_ng::from_str` has the same signature as `serde_yml::from_str`.
- No new capabilities; this is a security/maintenance fix only.

## Why (root cause, with evidence)

`Cargo.toml:116` declares `serde_yml = "0.0.12"`. This version is covered by
[RUSTSEC-2025-0068](https://rustsec.org/advisories/RUSTSEC-2025-0068.html): the
`serde_yml::ser::Serializer.emitter` API contains undefined behavior (can segfault), and
the crate is unmaintained. `cargo audit` / `cargo deny` flag it.

**Current exposure is non-trivial but narrow:**
- Only **2 call sites** exist, both in `src/tools/agent_defs.rs`, both using
  `serde_yml::from_str` (deserialization only):
  - `src/tools/agent_defs.rs:199` (production) — `serde_yml::from_str(&frontmatter)` parsing
    an agent-definition YAML frontmatter into `AgentDefinitionRaw`.
  - `src/tools/agent_defs.rs:284` (test) — `serde_yml::from_str(&front).unwrap()`.
- The vulnerable API is `Serializer.emitter` (serialization side). This codebase only
  deserializes, so the UB is **not reachable today**. But the crate is unmaintained and
  ships unsound code on the dependency graph; any future use of the serializer (or a
  transitive dependency reaching it) becomes UB, and `cargo audit` gates will block
  releases. The fix is to remove the unsound crate entirely.

The RustSec advisory recommends two maintained replacements: `serde_yaml_ng` and
`serde_norway` (both are forks of the original `serde_yaml`). This goal uses
**`serde_yaml_ng`** — it is the most widely-adopted community fork, its `from_str` API is
identical to `serde_yaml`/`serde_yml`, and for a 2-site `from_str`-only consumer the
migration is a pure rename.

## Scope (do exactly this, no more)

### 1. Swap the dependency in `Cargo.toml`

- Remove `serde_yml = "0.0.12"` (line 116).
- Add `serde_yaml_ng` with serde derive feature. Use a recent version; pin to a specific
  minor that's current at implementation time (e.g. `serde_yaml_ng = "1"` if available,
  otherwise the latest published — check `cargo search serde_yaml_ng` or the registry).
  If `serde_yaml_ng` requires `features = ["serde"]` to derive `Deserialize`, enable it;
  otherwise (it re-exports serde) no feature is needed. **Read the crate's own
  `Cargo.toml`/docs to confirm the version string and features before guessing.**

### 2. Update the 2 call sites in `src/tools/agent_defs.rs`

- `:199` — `serde_yml::from_str(&frontmatter)` → `serde_yaml_ng::from_str(&frontmatter)`.
- `:284` — `serde_yml::from_str(&front)` → `serde_yaml_ng::from_str(&front)`.
- The error type changes from `serde_yml::Error` to `serde_yaml_ng::Error`. Check the
  `.map_err(|e| Error::Config { ... })` at `:199` — if it formats the error via `Display`
  or `{e}`, no change needed (both errors impl `Display`). If it references the concrete
  `serde_yml::Error` type by name anywhere, update the import. Search the file for any
  other `serde_yml` references (use statements, type annotations) and update them.

### 3. Confirm `Cargo.lock` updates

- After the swap, `cargo build` will refresh `Cargo.lock` to remove `serde_yml` and add
  `serde_yaml_ng` (and possibly `unsafe-libyaml` vs whatever backend the fork uses).
  Commit the `Cargo.lock` change alongside the `Cargo.toml` + source change.
- Verify `serde_yml` no longer appears in `Cargo.lock`: `grep -c serde_yml Cargo.lock`
  should be `0`.

### 4. Tests

- The existing test at `agent_defs.rs:284` (which parses a sample frontmatter) is the
  regression guard — it must still pass with `serde_yaml_ng`. If the existing test
  fixtures use any YAML construct that `serde_yaml_ng` parses differently from
  `serde_yml` (extremely unlikely for simple frontmatter), the test will surface it.
- Add **one** focused test if no direct test currently exercises the production path at
  `:199` (the `:284` site is in a test fn but may not cover the exact `parse_agent_def`
  entrypoint). Check whether `parse_agent_def` / the function containing `:199` has a
  direct test; if not, add a small one that feeds a valid YAML frontmatter string and
  asserts the returned `AgentDefinition` has the expected fields. This locks the
  post-migration behaviour on the production path.

## Files NOT to touch

- Anything other than `Cargo.toml`, `Cargo.lock`, and `src/tools/agent_defs.rs`.
- Do NOT touch `src/run_core.rs`, `src/kernel.rs`, `src/runtime.rs`, any other tool, any
  crate under `crates/`.
- Do NOT add a `cargo audit` CI step (that's a separate concern; this goal only removes
  the flagged dep).
- `.dev/flows/`.

## Acceptance

- `cargo build --workspace` succeeds with `serde_yaml_ng` and WITHOUT `serde_yml`.
- `grep -c "serde_yml" Cargo.toml Cargo.lock src/` → all return `0`.
- `cargo test --workspace` green (including the `agent_defs` tests).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.
- (Optional, if `cargo-audit` is installed locally) `cargo audit` no longer reports
  RUSTSEC-2025-0068. If `cargo-audit` is not installed, skip this check — do NOT install
  it as part of this goal.

## Notes for the agent (traps)

- **API compatibility.** `serde_yaml_ng::from_str::<T>(s: &str) -> Result<T,
  serde_yaml_ng::Error>` is signature-compatible with `serde_yml::from_str`. The
  deserialized type (`AgentDefinitionRaw`) derives `Deserialize` via `serde` — `serde`
  itself does not change, only the YAML engine. Existing derive attributes stay as-is.
- **Error type in `map_err`.** At `:199` the code is `.map_err(|e| Error::Config { ... })`.
  Read what `Error::Config` expects: if it takes `String`/`Box<dyn Error>`/impl
  `Display`, the `{e}` formatting works identically. Do not change `Error::Config`'s
  definition — only the source crate.
- **Version string.** Don't hardcode a version you haven't verified exists. Run
  `cargo add serde_yaml_ng` (which resolves the latest compatible version) OR check the
  registry for the current version before writing `Cargo.toml`. If `cargo add` is
  available, prefer it — it writes the correct version + feature flags for you.
- **`unsafe-libyaml`.** `serde_yaml_ng` (and the original `serde_yaml`) use
  `unsafe-libyaml` as a backend. The RustSec advisory is about `serde_yml`'s
  *emitter* (serialization) UB, not `unsafe-libyaml`. Switching to `serde_yaml_ng` is the
  advisory's recommended remedy — do not second-guess it by hunting for an alternative
  backend.
- **No behaviour change expected.** This is a like-for-like YAML engine swap for a
  from_str-only consumer. If tests fail after the swap, the most likely cause is a
  version pin issue or a feature flag (serde derive), not a semantic difference. Check
  the error before assuming incompatibility.
- **Don't over-scope.** Do not migrate any other dependency, do not enable `cargo audit`
  in CI, do not touch `.dev/scripts/check-new-deps.sh`. This goal is the single dep swap.
  If you believe a journal entry is warranted, note the RUSTSEC ID and the one-line
  rationale.
