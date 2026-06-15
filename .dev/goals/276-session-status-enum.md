# Goal 276 — Type-safe SessionMeta.status enum

**Roadmap**: Phase 17 (Production Hardening) — P0 from
`docs/review/architecture-review-2026-06-15.md` (NEW-STORE-15,
also drift of 06-10 NEW-STORE-4)

**Design principle check**:
- Implemented as: introduce `enum SessionStatus` with
  `#[serde(rename_all = "lowercase")]`, replace all stringly-typed
  status fields in `SessionMeta` and `ExportedTranscript`.
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- ❌ Does NOT add a new feature flag

## Why

`src/session.rs` has two structs with `pub status: String`:

1. `SessionMeta::status` (line 342) — written as `"active"`,
   `"completed"`, `"interrupted"`, `"crashed"`, `"paused"` from
   at least 4 different call sites
2. `ExportedTranscript::status` (line 379) — same set

The status string has *no* type-level constraint. Adding a new
status (e.g. "stalled" for a future hang detection feature) means
grep-ing the codebase for every place that produces a status and
hoping for no typos. Last week's session_meta_schema_version work
(Goal 269) added a `schema_version` field; a typo'd status would
not surface until a `GET /sessions/:id` call returned unexpected
data.

The fix is mechanical: introduce a `SessionStatus` enum,
serialize lowercase, default to `Active` for old session files
without the field (already covered by `#[serde(default)]`).

## Scope (do exactly this, no more)

### 1. Define the enum

In `src/session.rs`, near the top of the file (after imports,
before `SESSION_SCHEMA_VERSION`):

```rust
/// Status of a session. Persisted as a lowercase string.
/// New variants are backward-compatible (old readers default to
/// `Active` when an unknown status is encountered).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Active,
    Completed,
    Interrupted,
    Crashed,
    Paused,
}

impl Default for SessionStatus {
    fn default() -> Self {
        Self::Active
    }
}

impl std::fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active => write!(f, "active"),
            Self::Completed => write!(f, "completed"),
            Self::Interrupted => write!(f, "interrupted"),
            Self::Crashed => write!(f, "crashed"),
            Self::Paused => write!(f, "paused"),
        }
    }
}
```

The set of variants is the union of every string literal
currently written. If grep finds a sixth string, add it before
proceeding.

### 2. Replace `status: String` with `status: SessionStatus`

In `SessionMeta` (line 342) and `ExportedTranscript` (line 379):

```rust
pub status: SessionStatus,
```

Add `#[serde(default)]` so old session files (where status was
missing or was an unknown string) still deserialize.

### 3. Update all write sites

grep for `"active".to_string()`, `"completed".to_string()`, etc.
in `src/`. Each occurrence becomes:

```rust
status: SessionStatus::Active,
status: SessionStatus::Completed,
// ...
```

Call sites are in: src/cli/resume.rs (multiple), src/session.rs
(own write paths), src/runtime.rs (RuntimeOutcome::close, if any
strings flow through). Find them by grep.

### 4. Update all read sites that compare strings

grep for `meta.status == "` and `meta.status != "` in src/.
Each becomes an enum comparison:

```rust
if meta.status == SessionStatus::Active { ... }
// (works directly because SessionStatus derives PartialEq)
```

### 5. Tests

In `src/session.rs` `mod tests`:

```rust
#[test]
fn session_status_serializes_lowercase() {
    let s = SessionStatus::Completed;
    let json = serde_json::to_string(&s).unwrap();
    assert_eq!(json, "\"completed\"");
}

#[test]
fn session_meta_unknown_status_deserializes_to_active() {
    // Old session file with status="stalled" (a hypothetical future
    // status this build doesn't know about) must NOT fail to load.
    let json = r#"{"schema_version":1,"status":"stalled","session_id":"x"}"#;
    // Build a minimal SessionMeta json; just assert it deserializes
    // and status defaults to Active. The exact field set may grow;
    // use serde_json::from_value on a Value with only `status`
    // populated + serde defaults for the rest.
}

#[test]
fn session_meta_active_roundtrip() {
    let meta = SessionMeta { status: SessionStatus::Active, ..test_default_meta() };
    let json = serde_json::to_string(&meta).unwrap();
    let restored: SessionMeta = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.status, SessionStatus::Active);
}
```

## Acceptance

- `cargo test --workspace` — green (existing + 3 new tests)
- `cargo clippy --all-targets --all-features -- -D warnings` —
  clean
- `cargo fmt --all` — applied
- `grep "pub status: String" src/session.rs` — 0 matches
- `grep "\"active\".to_string()\|\"completed\".to_string()\|\"interrupted\".to_string()\|\"crashed\".to_string()\|\"paused\".to_string()" src/` —
  0 matches (every write goes through the enum)
- `grep "meta\.status == \"\|\.status != \"" src/` — 0 matches
  (every compare goes through the enum)

## Notes for the agent

- The `Display` impl is used by `format!` callsites and CLI
  output. The `#[serde(rename_all = "lowercase")]` makes the JSON
  representation match the Display output.
- For `ExportedTranscript::status`, the field is also read by
  external SDK consumers. They will see a JSON `"status":
  "completed"` shape unchanged — the enum deserializes to that
  string. Backward compatible.
- If the union of variants turns out to differ between
  `SessionMeta::status` and `ExportedTranscript::status`, define
  ONE enum and use it in both. Don't fork.
- Estimated diff: 1 file mostly (session.rs), 2-3 small edits in
  cli/resume.rs and runtime.rs to use enum variants. ~80 lines
  net.
- **Test discipline reminder (from g268 post-mortem)**: prefer
  serde round-trip tests over runtime.

**Disjoint file guarantee**: This goal touches src/session.rs,
src/cli/resume.rs, src/runtime.rs (small). Goal 274 touches
src/http/handlers.rs. Goal 275 touches src/kernel.rs,
src/run_core.rs, src/runtime.rs (different lines), src/tools/mod.rs,
src/llm/mock.rs. The runtime.rs overlap is non-overlapping
methods — safe to run in parallel after quick coordination on
which methods are touched.