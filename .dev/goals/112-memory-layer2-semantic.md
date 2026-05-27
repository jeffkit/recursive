# Goal 112 — Memory Layer 2: Semantic facts with search

**Roadmap**: Phase 14.5 — Memory System (part 3/4)

**Design principle check**:
- Implemented as: enhanced `src/tools/memory.rs` (facts subsystem)
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- Builds on existing remember/recall/forget infrastructure

## Why

Working Memory (Layer 1) is a small whiteboard for current state.
Semantic Memory is the **long-term knowledge base** — facts extracted
from sessions that persist indefinitely, searchable across time.

This is where the agent accumulates expertise: "this project uses Tokio",
"user prefers small commits", "the compaction threshold is 200KB".

## Scope (do exactly this, no more)

### 1. Facts storage format

`<workspace>/.recursive/memory/facts.jsonl` (workspace-scoped)
`~/.recursive/memory/facts.jsonl` (global/user-scoped)

```rust
#[derive(Serialize, Deserialize, Clone)]
pub struct Fact {
    pub id: String,           // "F001", "F002", ...
    pub text: String,         // The fact content
    pub tags: Vec<String>,    // Categorization tags
    pub source: String,       // Session ID that created this
    pub created_at: String,   // ISO 8601
    pub last_accessed: Option<String>,  // Updated on recall
    pub access_count: u32,    // Popularity tracking
    pub superseded_by: Option<String>,  // If updated, points to newer fact
}
```

### 2. Enhanced tools

| Tool | Parameters | Description |
|------|-----------|-------------|
| `remember` | `text, tags[], scope?` | Store a new fact (dedup check first) |
| `recall` | `query, tags[]?, limit?, scope?` | Search facts by keyword + tags |
| `forget` | `id` | Mark fact as superseded (soft delete) |
| `update_fact` | `id, new_text` | Create new version, link via superseded_by |

`scope` parameter: `"workspace"` (default) or `"global"`.

### 3. Search implementation (pure Rust FTS)

No external dependencies. Simple but effective:

```rust
pub fn search_facts(facts: &[Fact], query: &str, tags: &[String], limit: usize) -> Vec<&Fact> {
    // 1. Tokenize query into words (lowercase, strip punctuation)
    // 2. For each fact, compute score:
    //    - term_frequency: count of query tokens found in fact.text
    //    - tag_match: bonus for each matching tag
    //    - recency_boost: 1.0 + (1.0 / days_old.max(1))
    //    - popularity_boost: 1.0 + (access_count as f64 * 0.1)
    //    - score = tf * tag_match * recency_boost * popularity_boost
    // 3. Filter out superseded facts
    // 4. Sort by score descending, take top `limit`
}
```

### 4. Deduplication on write

Before storing a new fact, check existing facts:
- Tokenize new fact and each existing fact
- Compute Jaccard similarity (intersection / union of token sets)
- If similarity > 0.7 with an existing fact:
  - If the new text is longer/more specific → supersede the old one
  - If the old text is more specific → reject the new one (return the old)

### 5. Eviction policy

Cap: 500 facts per scope (workspace or global).

When cap is reached:
1. Score all facts: `staleness = days_since_last_access * (1.0 / (access_count + 1))`
2. Evict the fact with highest staleness score
3. Evicted facts are removed from the JSONL (rewrite without them)

### 6. Recall injects into context

When agent calls `recall(query)`, results include:
```json
{
  "results": [
    {"id": "F042", "text": "Project uses Tokio multi-threaded scheduler", "relevance": 0.85},
    {"id": "F017", "text": "User prefers explicit error handling over ?", "relevance": 0.72}
  ],
  "total_facts": 142
}
```

### 7. Tests

- **Test A**: `remember` stores fact, `recall` finds it
- **Test B**: Duplicate detection (similar text → superseded)
- **Test C**: Tag filtering in recall
- **Test D**: Access count increments on recall
- **Test E**: Eviction triggers at cap (500), removes stalest fact
- **Test F**: `forget` marks fact as superseded
- **Test G**: `update_fact` creates new version with link
- **Test H**: Search scoring (more relevant = higher score)
- **Test I**: Global vs workspace scope isolation

## Acceptance

- `cargo build` green.
- `cargo test` green (9+ new tests).
- `cargo clippy --all-targets -- -D warnings` green.
- `recall("tokio")` in a project that has stored that fact returns it.
- Facts persist across sessions (verify by running twice).
- No new dependencies (pure Rust tokenization + scoring).

## Notes for the agent

- The old `remember`/`recall`/`forget` tools in memory.rs should be
  adapted, not duplicated. Refactor the existing implementations to use
  the new Fact struct and search logic.
- JSONL append for writes; full rewrite only on eviction or compaction.
- Tokenization: split on whitespace + punctuation, lowercase, ignore
  common stop words (a, the, is, of, in, to, and, for, it).
- Keep stop word list minimal (< 30 words, hardcoded).
- The `source` field should be the current session_id if available,
  otherwise "manual" or "import".
