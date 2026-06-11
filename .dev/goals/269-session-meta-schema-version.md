# Goal 269 — SessionMeta schema_version field

**Roadmap**: Phase 17 (Production Hardening) — P0 from
`docs/review/architecture-review-2026-06-10.md` (NEW-STORE-4)

**Design principle check**:
- Implemented as: add `schema_version: u32` field to `SessionMeta`
  with `#[serde(default = "default_schema_version")]`, write the
  current value in `SessionWriter::create`, refuse to load unknown
  versions in `SessionReader::load_meta`
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- ❌ Does NOT add a new feature flag

## Why

`SessionMeta` (src/session.rs:300) lacks a `schema_version` field.
The only schema-versioning that exists is the legacy `SessionFile`
struct (src/session.rs:29) — it has `SCHEMA_VERSION: u32 = 1` but
the new JSONL pipeline (Phase 14) never adopted it. `SessionMeta`
fields rely on `#[serde(default)]` discipline to be loadable from
older session files. If a future version drops a field without
`default`, every pre-existing session fails to load and silently
disappears from the session list (lost user data, no diagnostic).

This is preventive maintenance, not a regression: today, a missing
field would be loadable thanks to `#[serde(default)]`. But there
is no enforcement that we are reading a schema we understand.
Adding `schema_version` makes the field explicit and gives a single
check point (load) to reject (or migrate) future-incompatible
sessions.

## Scope (do exactly this, no more)

### 1. Add `schema_version: u32` to `SessionMeta`

In `src/session.rs`, modify the `SessionMeta` struct (line 300 area)
to add:

```rust
pub struct SessionMeta {
    /// Schema version of the persisted SessionMeta. Bump this whenever
    /// a non-backward-compatible field is added (e.g. dropping a
    /// `#[serde(default)]` attribute). The on-disk format is read by
    /// `SessionReader::load_meta` which checks this field and refuses
    /// to load a session with a `schema_version` it does not
    /// understand.
    ///
    /// Current value: `1` (introduced in Goal 269, 2026-06-11).
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    // ... existing fields unchanged ...
}
```

And the helper (placed near the struct, e.g. line 290 area):

```rust
fn default_schema_version() -> u32 { 1 }
```

### 2. Write the value in `SessionWriter::create`

In `src/session.rs::SessionWriter::create` (around line 600 area, the
function that returns `Self { ... }`), include
`schema_version: 1` in the constructed `SessionMeta`. (Or use the
helper `default_schema_version()`.)

### 3. Check the value in `SessionReader::load_meta`

In `src/session.rs::SessionReader::load_meta` (around line 700 area,
the function that reads `.meta.json` and deserializes into
`SessionMeta`), after the `serde_json::from_slice` succeeds, add:

```rust
const SUPPORTED_SCHEMA_VERSION: u32 = 1;
if meta.schema_version > SUPPORTED_SCHEMA_VERSION {
    tracing::warn!(
        "session {} has schema_version={} > supported={}, skipping",
        session_id, meta.schema_version, SUPPORTED_SCHEMA_VERSION
    );
    return Err(SessionError::SchemaTooNew { ... });
}
```

Choose a clear error variant. If a new variant is needed in
`src/error.rs`, add it; do not reuse an unrelated variant.

**Acceptance behavior for old (pre-Goal-269) sessions**: the field
defaults to 1 via `#[serde(default = ...)]`, so load succeeds.
No migration needed.

### 4. Tests

- **test_session_meta_round_trip** — Create a `SessionMeta` with
  `schema_version: 1`, serialize to JSON, deserialize back, assert
  the field round-trips. In `src/session.rs` `mod tests`.
- **test_session_meta_default_schema_version** — Serialize a
  `SessionMeta` **without** setting `schema_version` (use the
  struct-update syntax to omit the field), then deserialize and
  assert the field defaults to 1. Proves `#[serde(default)]` works
  for backward compat.
- **test_load_rejects_future_schema_version** — Write a JSON blob
  with `schema_version: 999` to a tempdir, attempt to load via
  `SessionReader::load_meta`, assert it returns the new error
  variant.

If `SessionReader::load_meta` is hard to instantiate in a unit
test, write the round-trip tests only and put a `tracing::warn`
plus a `// TODO: enforce in load_meta` comment in the production
code. Mark the test file as `#[cfg(test)] mod tests` at the
bottom of `src/session.rs`.

### 5. No changes outside `src/session.rs` and `src/error.rs`

Do not touch any other file. The goal is contained to two
adjacent modules.

## Acceptance

- `cargo test --workspace` — green (existing + new tests)
- `cargo clippy --all-targets --all-features -- -D warnings` — clean
- `cargo fmt --all` — applied
- `grep "schema_version" src/session.rs` — matches ≥3 (the field
  declaration, the helper, the write site, the check site, the
  test assertions)
- A new test case in `src/session.rs` covers (a) round-trip and
  (b) default for old sessions. The future-version rejection test
  is a stretch goal — skip if `SessionReader` setup is heavy.

## Notes for the agent

- The minimum viable change is **just the field + helper + write
  site** (steps 1 + 2). The check at load time (step 3) is what
  makes the field load-bearing — without it, `schema_version` is
  decorative. If you only do 1+2, the goal is half-done; do 1+2+3.
- The error variant addition in `src/error.rs` is a real error
  variant. The existing variants list (around line 30) has many
  patterns; add `SchemaTooNew { session_id: String, found: u32,
  supported: u32 }` following the same style. Do NOT collapse into
  `Other`.
- Do NOT bump `schema_version` to 2 anywhere. The current value
  is 1; this goal is **adding** the field, not changing existing
  on-disk data. Old sessions read with `schema_version: 1`
  (default) are valid.
- Do NOT change any `#[serde(default = ...)]` attribute on
  existing fields. That is a separate refactor; touching it is
  scope creep.
- Estimated diff: 2 files, ~30-50 lines (1 field + 1 helper + 1
  write site + 1 check + 2-3 tests + 1 error variant).
- **Source-grep snapshot is acceptable for the future-version
  rejection test** if SessionReader setup is too heavy for a unit
  test. Grep for `"schema_version"` in the function body.

**Test discipline reminder (from g268 post-mortem)**:
- Do NOT spawn a worker task in a test that holds a permit, a
  MockProvider, or anything that can deadlock. Use
  serde-round-trip tests (deterministic) or source-grep snapshots
  (no runtime).
- Do NOT call `runtime.run(...)` in a test. Use
  `serde_json::from_value` or `from_slice` directly.
- If you find yourself writing a `tokio::spawn` in a test, stop
  and use a simpler test form.
