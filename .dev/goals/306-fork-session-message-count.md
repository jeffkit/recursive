# Goal 306 — Fix fork_session message_count inconsistency (total vs non-system)

**Roadmap**: Post-Phase (API consistency)

**Design principle check**:
- Implemented as: using `non_system_count` instead of `transcript_snapshot.len()`
  as `message_count` in `ForkSessionResponse` in `src/http/handlers.rs`
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

`POST /sessions/:id/fork` returns a `ForkSessionResponse` with a
`message_count` field set to `transcript_snapshot.len()` — the **total**
number of messages including the system prompt.

However, `GET /sessions` returns `SessionInfo.message_count` = the
**non-system** message count (read from `non_system_message_count` AtomicUsize).

This creates an inconsistency: if you fork a session that had 10 user+assistant
messages (and 1 system prompt), the fork response says `"message_count": 11`
but a subsequent `GET /sessions` for the forked session returns
`"message_count": 10`.

The fix is trivial: in `fork_session`, the variable `non_system_count` is
already computed (it's used to initialize `non_system_message_count`). Simply
use `non_system_count` instead of `transcript_snapshot.len()` as the
`message_count` in the response.

## Scope (do exactly this, no more)

### 1. `src/http/handlers.rs` — fix `fork_session`

Find (around line 575):
```rust
Json(ForkSessionResponse {
    id: new_id,
    created_at,
    message_count,
})
```

The `message_count` variable was set at line ~532:
```rust
let message_count = transcript_snapshot.len();
```

Remove the `message_count` variable (or leave it unused) and use
`non_system_count` in the response instead:
```rust
Json(ForkSessionResponse {
    id: new_id,
    created_at,
    message_count: non_system_count,
})
```

Also update the `ForkSessionResponse.message_count` doc comment to say:
"Number of non-system messages copied from the source session (matches
the semantics of `SessionInfo.message_count` from `GET /sessions`)."

### 2. Remove or keep `message_count` variable

If the `let message_count = transcript_snapshot.len();` variable is
no longer used, remove it to keep the code clean. `cargo clippy` will
flag it as an unused variable otherwise.

### 3. Tests

Add a test in `tests/http.rs` (or the `#[cfg(test)]` section in
`src/http/handlers.rs`) that:
1. Creates a session
2. Sends one message (so there's 1 non-system message + 1 system prompt = 2 total)
3. Forks the session
4. Verifies the fork response `message_count` is 1 (non-system only),
   not 2 (total)

If setting up a full HTTP test is complex, a simpler unit test
that directly tests the count logic is acceptable.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `ForkSessionResponse.message_count` equals `non_system_count` (non-system
  messages only), matching the semantics of `SessionInfo.message_count`

## Notes for the agent

- Read `src/http/handlers.rs` around lines 515–585 for the full
  `fork_session` function.
- `non_system_count` is already computed just above the response construction.
- `message_count` (the total variable) can be removed if clippy flags it.
- The `ForkSessionResponse` struct is at line ~499.
- **DO NOT modify** `src/agent.rs`, `src/runtime.rs`, `src/kernel.rs`,
  `src/http/mod.rs` (beyond confirming struct), or any non-HTTP files.
