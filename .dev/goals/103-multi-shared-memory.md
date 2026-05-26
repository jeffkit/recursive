# Goal 103 — Multi-Agent: Shared Project Memory

**Roadmap**: Phase 13.2 — Multi-Agent Framework (part 2/5)

**Design principle check**:
- Implemented as: extension to `src/multi.rs` — shared memory store
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- Multi-agent shares memory, not transcript state (design principle #6)

## Why

When multiple agents work on the same project, they need a shared
knowledge base — decisions made, files modified, context discovered.
This shared memory is separate from individual agent transcripts
(which are private to each run).

## Scope (do exactly this, no more)

### 1. `src/multi.rs` — add SharedMemory

```rust
use tokio::sync::RwLock;
use std::sync::Arc;

/// Shared memory store for multi-agent coordination.
/// Thread-safe, async-compatible key-value store.
#[derive(Clone)]
pub struct SharedMemory {
    store: Arc<RwLock<HashMap<String, MemoryEntry>>>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct MemoryEntry {
    pub key: String,
    pub value: String,
    pub author: String,      // role name that wrote this
    pub timestamp: u64,      // unix timestamp
}

impl SharedMemory {
    pub fn new() -> Self {
        Self { store: Arc::new(RwLock::new(HashMap::new())) }
    }

    /// Store a key-value pair.
    pub async fn set(&self, key: String, value: String, author: String) {
        let entry = MemoryEntry {
            key: key.clone(),
            value,
            author,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };
        self.store.write().await.insert(key, entry);
    }

    /// Get a value by key.
    pub async fn get(&self, key: &str) -> Option<MemoryEntry> {
        self.store.read().await.get(key).cloned()
    }

    /// List all keys.
    pub async fn keys(&self) -> Vec<String> {
        self.store.read().await.keys().cloned().collect()
    }

    /// Get all entries (for context injection).
    pub async fn all(&self) -> Vec<MemoryEntry> {
        self.store.read().await.values().cloned().collect()
    }

    /// Remove a key.
    pub async fn remove(&self, key: &str) -> bool {
        self.store.write().await.remove(key).is_some()
    }

    /// Format all entries as a context string for injection into prompts.
    pub async fn to_context_string(&self) -> String {
        let entries = self.all().await;
        if entries.is_empty() {
            return String::new();
        }
        let mut lines = vec!["## Shared Project Memory".to_string()];
        for entry in &entries {
            lines.push(format!("- **{}** (by {}): {}", entry.key, entry.author, entry.value));
        }
        lines.join("\n")
    }

    /// Number of entries.
    pub async fn len(&self) -> usize {
        self.store.read().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.store.read().await.is_empty()
    }
}
```

### 2. Integrate with AgentPool

Add `memory: SharedMemory` to AgentPool:

```rust
pub struct AgentPool {
    roles: HashMap<String, AgentRole>,
    provider: Arc<dyn LlmProvider>,
    config: Config,
    memory: SharedMemory,  // NEW
}
```

In `run_with_role`, inject memory context into the system prompt:

```rust
pub async fn run_with_role(&self, role_name: &str, goal: &str) -> Result<AgentOutcome, Error> {
    let role = ...;
    let memory_context = self.memory.to_context_string().await;
    let system_prompt = if memory_context.is_empty() {
        role.system_prompt.clone()
    } else {
        format!("{}\n\n{}", role.system_prompt, memory_context)
    };
    // ... build agent with system_prompt ...
}
```

Add accessor: `pub fn memory(&self) -> &SharedMemory`

### 3. Tests

- Test: SharedMemory set + get works
- Test: SharedMemory keys() returns all keys
- Test: SharedMemory remove works
- Test: to_context_string formats correctly
- Test: empty memory returns empty string
- Test: AgentPool includes memory in system prompt

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean

## Notes for the agent

- Read `src/multi.rs` for current AgentPool implementation.
- SharedMemory must be Clone (it wraps Arc<RwLock<...>>).
- Use `tokio::sync::RwLock` (not std) for async compatibility.
- **DO NOT modify `src/agent.rs`.**
- **DO NOT add file persistence — in-memory only.**
