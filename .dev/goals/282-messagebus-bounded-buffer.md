# Goal 282 — MessageBus bounded ring buffer

**Roadmap**: Phase 17 (Production Hardening) — P1 from
`docs/review/architecture-review-2026-06-15.md` (NEW-MEM-15),
also referenced in 06-10 NEW-KERN-5.

**Design principle check**:
- Implemented as: replace `Vec<AgentMessage>` in `MessageBus`
  with a bounded `VecDeque<AgentMessage>`, evict oldest on
  overflow.
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- ❌ Does NOT add a new feature flag

## Why

`src/multi.rs:141-144`:

```rust
pub struct MessageBus {
    messages: Arc<RwLock<Vec<AgentMessage>>>,
    subscribers: Arc<RwLock<HashMap<String, broadcast::Sender<AgentMessage>>>>,
}
```

The `messages` history grows unbounded. Every `send()` (line 156)
appends; nothing evicts. A long-running `AgentPool` with many
agents (e.g. a 6-hour orchestration that emits a
status message every 30s) accumulates ~720 messages per agent.
With 8 agents × 720 = ~5760 messages × ~200 bytes/msg = ~1.2MB
held in memory.

Worse: any caller that asks for `MessageBus::history()` (or
similar — verify exists) loads the whole thing. LLM context
budget is finite; an unbounded history means any historical
recall can blow past the budget, triggering 400 errors that
look like provider problems but are actually caller issues.

The fix is a bounded ring buffer: replace `Vec` with
`VecDeque`, set a capacity (e.g. 1000 messages), evict oldest
on overflow.

## Scope (do exactly this, no more)

### 1. Add bounded field

In `src/multi.rs:141-160`:

```rust
pub struct MessageBus {
    /// Bounded ring buffer of recent messages. Oldest evicted
    /// on overflow. Capacity is `MESSAGE_BUS_CAPACITY` to bound
    /// memory in long-running multi-agent pools.
    messages: Arc<RwLock<VecDeque<AgentMessage>>>,
    subscribers: Arc<RwLock<HashMap<String, broadcast::Sender<AgentMessage>>>>,
}

/// Maximum messages retained in `MessageBus.messages` history.
/// 1000 messages × ~200 bytes/msg ≈ 200 KiB — bounded for
/// long-running pools while still giving plenty of recent
/// context for the goal-judge and history inspection.
pub const MESSAGE_BUS_CAPACITY: usize = 1000;
```

### 2. Update `new()`

```rust
pub fn new() -> Self {
    Self {
        messages: Arc::new(RwLock::new(VecDeque::with_capacity(MESSAGE_BUS_CAPACITY))),
        subscribers: Arc::new(RwLock::new(HashMap::new())),
    }
}
```

### 3. Update `send()`

In `src/multi.rs:155-165`, replace the push:

```rust
pub async fn send(&self, msg: AgentMessage) {
    {
        let mut history = self.messages.write().await;
        if history.len() == MESSAGE_BUS_CAPACITY {
            history.pop_front();
        }
        history.push_back(msg.clone());
    }
    let subs = self.subscribers.read().await;
    if msg.to == "broadcast" {
        for tx in subs.values() {
            let _ = tx.send(msg.clone());
        }
    } else if let Some(tx) = subs.get(&msg.to) {
        let _ = tx.send(msg);
    }
}
```

### 4. Add `history()` getter (if missing)

If `MessageBus` already exposes a `history()` method that returns
the full Vec, change its return type to `VecDeque<AgentMessage>`
(or clone into `Vec` for callers — pick whichever is less
invasive to the call sites). If there's no getter, skip this step.

### 5. Add a `with_capacity` constructor for tests

```rust
impl MessageBus {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            messages: Arc::new(RwLock::new(VecDeque::with_capacity(cap))),
            subscribers: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}
```

### 6. Tests

In `src/multi.rs` `mod tests`:

```rust
#[tokio::test]
async fn message_bus_evicts_oldest_on_overflow() {
    let bus = MessageBus::with_capacity(3);
    for i in 0..5 {
        bus.send(AgentMessage {
            from: format!("a{i}"),
            to: "broadcast".into(),
            content: format!("msg-{i}"),
            message_type: MessageType::Status,
        }).await;
    }
    let history = bus.messages.read().await;
    let contents: Vec<_> = history.iter().map(|m| m.content.clone()).collect();
    assert_eq!(contents, vec!["msg-1", "msg-2", "msg-3", "msg-4"]);
    // msg-0 was evicted; msg-1..4 retained (newest 3 + the one
    // that pushed out msg-0).
    assert_eq!(history.len(), 3);
}

#[tokio::test]
async fn message_bus_default_capacity_is_bounded() {
    let bus = MessageBus::new();
    assert_eq!(MESSAGE_BUS_CAPACITY, 1000);
}
```

## Acceptance

- `cargo test --workspace` — green (existing + 2 new tests)
- `cargo clippy --all-targets --all-features -- -D warnings` —
  clean
- `cargo fmt --all` — applied
- `grep "Vec<AgentMessage>" src/multi.rs` — 0 matches (replaced
  with VecDeque)
- `grep "MESSAGE_BUS_CAPACITY" src/multi.rs` — ≥ 3 matches:
  constant, `new`, test

## Notes for the agent

- The `subscribers` field uses a `broadcast::Sender` channel,
  which is *also* bounded (default 1024 — verify). If it's
  silently dropping messages already, the eviction behavior is
  not new. Don't change `subscribers` here; the goal is only
  the `messages` history.
- If `MessageBus::history()` or similar returns `Vec<AgentMessage>`,
  change it to `VecDeque<AgentMessage>` and update call sites
  — but most call sites that index into it (`history[i]`) work
  identically on `VecDeque`. Use `make_contiguous()` if you need
  a slice.
- If a goal-judge or other consumer reads `history` expecting
  ALL messages ever sent, it now gets only the most recent
  MESSAGE_BUS_CAPACITY. Audit those consumers; they may need to
  switch to subscribing to live events instead of polling
  history.
- Estimated diff: 1 file (multi.rs), ~30 lines net.
- **Test discipline reminder (from g268 post-mortem)**: prefer
  direct field-level tests over spinning up an AgentPool.

**Disjoint file guarantee**: This goal touches only src/multi.rs.
Safe to run in parallel with every other goal in this batch.