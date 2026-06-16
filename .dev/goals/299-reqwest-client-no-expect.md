# Goal 299 — Document construction-phase carve-out for reqwest Client::build() expect()

**Roadmap**: Post-Phase (Invariant #5 compliance / documentation)

**Design principle check**:
- Implemented as: adding a `// construction carve-out` comment to the
  `expect()` calls in `WebFetch::new()` and `WebSearch::new()`, matching
  the pattern established in `src/providers.rs`
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

`src/tools/web_fetch.rs` line 32 and `src/tools/web_search.rs` line 73 both
call `.expect("reqwest client build")` in their public constructors. At face
value this appears to violate **Invariant #5** (no `unwrap()`/`expect()` in
non-test product code).

However, this is a legitimate **construction-phase carve-out**: if the TLS
backend fails to initialize, the agent cannot perform any HTTP requests at
all. This is equivalent to the `providers.rs` case (line ~62), which also
uses `.expect()` for "parse bundled TOML" — a fatal condition if the binary
is corrupted — and has the explicit comment:

```rust
// "construction" carve-out applies. We keep it inside `all_presets`
// (not in `bundled_presets`) so the static-unwrap checker does not
// flag the .expect as "new" code ...
```

**Important**: replacing `.expect("reqwest client build")` with
`.unwrap_or_else(|_| Client::new())` is NOT a valid fix because
`Client::new()` internally calls `ClientBuilder::new().build().expect("Client::new()")`.
The fallback would panic in the same conditions, just with a less informative
message. The only real options are:
1. Accept the construction-phase panic (this goal) — simplest, consistent with `providers.rs`
2. Change `new()` → `Result<Self>` (larger refactor, separate goal)

This goal implements option 1: add a clear comment to each `.expect()` call
explaining it is an intentional construction-phase carve-out from Invariant #5.

## Scope (do exactly this, no more)

### 1. `src/tools/web_fetch.rs` — add construction carve-out comment

Before the `.build().expect(...)` in `WebFetch::new()`, add a comment:
```rust
// Construction carve-out: if TLS backend fails to initialize, the process
// cannot perform HTTP requests at all. This is a fatal startup condition
// equivalent to the providers.rs TOML parse (Invariant #5 §construction).
.build()
.expect("reqwest client build: TLS backend unavailable")
```

Change the `expect` message from `"reqwest client build"` to
`"reqwest client build: TLS backend unavailable"` for clarity.

### 2. `src/tools/web_search.rs` — same pattern

Same comment and same improved expect message for `WebSearch::new()`.

Do NOT change the `#[cfg(test)]` `with_test_base` constructor.

### 3. Tests

Add a unit test in each file that verifies construction succeeds in a
normal environment (smoke test only — no forced-failure path):

In `src/tools/web_fetch.rs` `#[cfg(test)]`:
```rust
#[test]
fn web_fetch_construction_smoke() {
    let tool = WebFetch::new();
    assert_eq!(tool.spec().name, "WebFetch");
}
```

In `src/tools/web_search.rs` `#[cfg(test)]`:
```rust
#[test]
fn web_search_construction_smoke() {
    let tool = WebSearch::new();
    assert_eq!(tool.spec().name, "WebSearch");
}
```

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- Both `web_fetch.rs` and `web_search.rs` have the construction carve-out
  comment before their `.expect()` calls
- The `expect` messages are updated to include "TLS backend unavailable"

## Notes for the agent

- Read both `src/tools/web_fetch.rs` and `src/tools/web_search.rs` before editing.
- Read `src/providers.rs` around line 55-65 to see the exact pattern of the
  `providers.rs` construction carve-out comment, and mirror it.
- DO NOT use `unwrap_or_else(|_| Client::new())` — `Client::new()` also panics
  if TLS init fails, making that pattern a no-op fix.
- **DO NOT modify** `src/agent.rs`, `src/runtime.rs`, `src/http/`, or any
  other tools beyond adding the two smoke tests.
