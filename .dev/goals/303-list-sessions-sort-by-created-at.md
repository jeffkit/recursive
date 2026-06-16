# Goal 303 — Sort GET /sessions results by created_at instead of session ID

**Roadmap**: Post-Phase (API usability)

**Design principle check**:
- Implemented as: change sort key in `list_sessions` handler in
  `src/http/handlers.rs`
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

`GET /sessions` currently sorts results by `session_id` (which is a UUID, i.e.
random). The original comment says this was for "stable, deterministic
pagination across requests" — but a random-UUID sort order is stable without
being *useful*: clients receive sessions in a random order that doesn't
correspond to any meaningful sequence.

The `created_at` field is ISO 8601 (format `YYYY-MM-DDTHH:MM:SSZ`) which
sorts lexicographically into chronological order. Sorting by `created_at`
gives:
- **Chronological ordering**: clients see oldest sessions first (most
  common expectation for paginated list APIs like GitHub, Linear, etc.)
- **Still deterministic**: ISO 8601 strings are stable across repeated calls
- **Tiebreaker**: two sessions created in the same second use `id` as a
  secondary key for full determinism

## Scope (do exactly this, no more)

### 1. `src/http/handlers.rs` — fix sort in `list_sessions`

Find (around line 325):
```rust
// Sort by session_id for stable, deterministic pagination across requests.
// Without this, HashMap iteration order is non-deterministic and pages
// would shift between calls.
infos.sort_by(|a, b| a.id.cmp(&b.id));
```

Replace with:
```rust
// Sort by creation time (ISO 8601 lexicographic = chronological) so clients
// receive sessions in a predictable, meaningful order. Use `id` as a secondary
// key to break ties between sessions created in the same second.
infos.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
```

### 2. Tests

In the existing `#[cfg(test)]` module in `src/http/handlers.rs`, add a test
that:
1. Calls `list_sessions` with 3 sessions having different `created_at` values
2. Verifies the response order is chronological (oldest first)

If the existing test infrastructure for `list_sessions` is complex (e.g.
requires a live AppState), a simpler unit test on the sorting logic alone
is acceptable:

```rust
#[test]
fn list_sessions_sort_is_chronological() {
    let mut infos = vec![
        SessionInfo { id: "c".into(), created_at: "2026-01-03T00:00:00Z".into(), message_count: 0, title: None },
        SessionInfo { id: "a".into(), created_at: "2026-01-01T00:00:00Z".into(), message_count: 0, title: None },
        SessionInfo { id: "b".into(), created_at: "2026-01-02T00:00:00Z".into(), message_count: 0, title: None },
    ];
    infos.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    assert_eq!(infos[0].id, "a");
    assert_eq!(infos[1].id, "b");
    assert_eq!(infos[2].id, "c");
}
```

Also add a test for the same-second tiebreaker:
```rust
#[test]
fn list_sessions_same_second_tiebreak_by_id() {
    let mut infos = vec![
        SessionInfo { id: "z".into(), created_at: "2026-01-01T00:00:00Z".into(), message_count: 0, title: None },
        SessionInfo { id: "a".into(), created_at: "2026-01-01T00:00:00Z".into(), message_count: 0, title: None },
    ];
    infos.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    assert_eq!(infos[0].id, "a");
    assert_eq!(infos[1].id, "z");
}
```

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `GET /sessions` returns sessions sorted oldest-first by `created_at`

## Notes for the agent

- Read `src/http/handlers.rs` around line 306–345 for the full `list_sessions`
  function.
- `SessionInfo` struct is defined in `src/http/mod.rs` — confirm it has a
  `created_at: String` field.
- The `SessionInfo` struct is `pub(super)` — it should be accessible from
  tests in `handlers.rs`.
- **DO NOT modify** `src/agent.rs`, `src/runtime.rs`, `src/kernel.rs`,
  `src/http/mod.rs` (except confirming struct definition), or any non-HTTP files.
