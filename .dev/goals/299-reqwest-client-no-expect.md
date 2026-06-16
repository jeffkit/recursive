# Goal 299 — Fix reqwest Client::build() expect() in WebFetch and WebSearch

**Roadmap**: Post-Phase (Invariant #5 compliance)

**Design principle check**:
- Implemented as: replacing `expect()` calls in `WebFetch::new()` and
  `WebSearch::new()` with safe fallback using `unwrap_or_else`
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

`src/tools/web_fetch.rs` line 32 and `src/tools/web_search.rs` line 73 both
call `.expect("reqwest client build")` in their public constructors. This
violates **Invariant #5** (no `unwrap()`/`expect()` in non-test product code).

While `reqwest::Client::builder().build()` rarely fails in practice, under
unusual environments (restrictive sandboxes, TLS backend issues, fuzz testing)
it can, causing the entire agent process to panic in an unrecoverable way.

The fix is safe and minimal: replace `.expect("reqwest client build")` with
`.unwrap_or_else(|_| Client::new())`. `Client::new()` is documented as the
"simplest client" that always succeeds. The only downside is that the
user-agent and timeout customizations are lost, which is acceptable as a
panic-free fallback.

Note: The `with_test_base` constructor in `web_search.rs` is `#[cfg(test)]`
and thus exempt from Invariant #5.

## Scope (do exactly this, no more)

### 1. `src/tools/web_fetch.rs`

At `WebFetch::new()`, line ~32:
```rust
// Before
.build()
.expect("reqwest client build");

// After
.build()
.unwrap_or_else(|_| Client::new());
```

### 2. `src/tools/web_search.rs`

At `WebSearch::new()`, line ~73:
```rust
// Before
.build()
.expect("reqwest client build");

// After
.build()
.unwrap_or_else(|_| Client::new());
```

Do NOT change the `#[cfg(test)]` `with_test_base` constructor.

### 3. Tests

Add a unit test in `src/tools/web_fetch.rs` (in the existing `#[cfg(test)]`
module) that simply constructs `WebFetch::new()` and verifies the tool spec
name is correct — this acts as a smoke test that construction doesn't panic:

```rust
#[test]
fn web_fetch_new_does_not_panic() {
    let tool = WebFetch::new();
    assert_eq!(tool.spec().name, "WebFetch");
}
```

Similarly add to `src/tools/web_search.rs`:
```rust
#[test]
fn web_search_new_does_not_panic() {
    let tool = WebSearch::new();
    assert_eq!(tool.spec().name, "WebSearch");
}
```

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- No `.expect()` calls remain in the non-test paths of `web_fetch.rs`
  and `web_search.rs`
- `grep -n "expect" src/tools/web_fetch.rs src/tools/web_search.rs` shows
  no `expect()` outside `#[cfg(test)]` blocks

## Notes for the agent

- Read both `src/tools/web_fetch.rs` and `src/tools/web_search.rs` before editing.
- Confirm the `WebSearch::with_test_base` fn is inside `#[cfg(test)]` before
  leaving it unchanged.
- The `unwrap_or_else(|_| Client::new())` idiom is the canonical Rust pattern
  for "try with options, fall back to default".
- **DO NOT modify** `src/agent.rs`, `src/runtime.rs`, `src/http/`, any other
  tools, or test files beyond adding the two smoke tests.
