# Goal 290 — RateLimiter: prune idle token buckets to prevent unbounded growth

**Roadmap**: Post-Phase (Arch-review cleanup) — perf/correctness issue

**Design principle check**:
- Implemented as: add a `prune()` method to `RateLimiter` that removes
  fully-refilled (idle) buckets, and call it from `AppState::session_reaper`
  on the same timer tick.
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- ❌ Does NOT add a new feature flag

## Why

`src/http/rate_limit.rs`: `RateLimiter` stores `Arc<Mutex<HashMap<String,
TokenBucket>>>`. The `check()` method inserts a new `TokenBucket` for every
unique `key` (API key or remote IP) it encounters, but never removes old
entries. In a public deployment with many distinct callers (or after IP
spoofing), the map grows indefinitely — a memory leak.

A bucket is "idle" when its tokens have refilled to full capacity:
`bucket.tokens >= capacity`. Such a bucket has not received a request
recently enough to drain it below maximum. We can safely evict it: the
next request from that key will re-insert a fresh full bucket, which
is identical to the restored state.

## Scope (do exactly this, no more)

### 1. `src/http/rate_limit.rs` — add `pub(super) fn prune(&self)`

```rust
/// Remove idle token buckets (tokens fully refilled to capacity).
/// Safe to drop: a re-arriving client gets a fresh full bucket,
/// which is the same as the stored state.
pub(super) async fn prune(&self) {
    let mut buckets = self.buckets.lock().await;
    buckets.retain(|_, b| b.tokens < self.capacity as f64);
}
```

### 2. `src/http/mod.rs` — call `rate_limiter.prune()` from `session_reaper`

Find the `session_reaper` async task (the one that evicts old sessions on
a `tokio::time::interval`). After the session-eviction block, add:

```rust
state.rate_limiter.prune().await;
```

This piggybacks on the existing 60-second (or configurable) reaper interval
with no new timer.

### 3. Tests

Add a unit test in `src/http/rate_limit.rs`:

```rust
#[tokio::test]
async fn prune_removes_full_buckets() {
    let limiter = RateLimiter::new(2, 1.0);
    // Create an entry and drain it partially
    limiter.check("client-a").await;
    // Create a fresh entry (full bucket)
    limiter.check("client-b").await; // partial drain
    limiter.check("client-new-full").await; // drain from full → partial
    // At this point all entries are partially drained
    // … (alternative: create a bucket and wait for full refill)
    // Simpler: just verify prune() doesn't panic and removes nothing
    // when all buckets are partially drained
    limiter.prune().await;
    // All were partially drained → none removed
    let count_after = limiter.bucket_count().await;
    assert_eq!(count_after, 3);

    // Now manually insert a full bucket to test eviction
    {
        let mut b = limiter.buckets.lock().await;
        b.insert("idle-client".to_string(), TokenBucket {
            tokens: 2.0, // = capacity → idle
            last_refill: std::time::Instant::now(),
        });
    }
    limiter.prune().await;
    // idle-client evicted, 3 partial buckets remain
    assert_eq!(limiter.bucket_count().await, 3);
}
```

Add `pub(super) async fn bucket_count(&self) -> usize` helper for the test.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `RateLimiter::prune()` exists and is called from `session_reaper`
- At least one test covers prune behaviour

## Notes for the agent

- Read `src/http/rate_limit.rs` and `src/http/mod.rs::session_reaper` first.
- The `TokenBucket` struct may be private — you may need to add a pub(crate)
  test-helper constructor or test via the `prune()` path directly.
- Keep `prune()` `pub(super)` (same visibility as the existing `check()`
  which is also `pub(super)` implicitly through use in the middleware).
- No new crate dependencies.
- **DO NOT modify** `src/agent.rs`, `src/runtime.rs`, `src/kernel.rs`.
