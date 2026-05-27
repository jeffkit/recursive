# Goal 113 — Memory Layer 3: Episodic recall (session search)

**Roadmap**: Phase 14.5 — Memory System (part 4/4)

**Design principle check**:
- Implemented as: new tool `session_search` in `src/tools/` + index builder
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- Depends on Goal 107-108 (sessions stored as JSONL)

## Why

Sometimes the agent needs to recall "what did we discuss about X last
week?" or "how was this bug fixed before?". Episodic memory enables
searching across historical sessions — not just the current one.

This is the equivalent of a developer grepping their shell history or
chat logs.

## Scope (do exactly this, no more)

### 1. Session index

Built/updated when a session finishes (in `SessionWriter::finish`):

`~/.recursive/sessions/<workspace-slug>/index.jsonl`

```rust
#[derive(Serialize, Deserialize)]
pub struct SessionIndex {
    pub session_id: String,
    pub goal: String,
    pub summary: Option<String>,  // Generated on finish (first + last assistant msgs)
    pub model: String,
    pub created_at: String,
    pub finished_at: String,
    pub status: String,
    pub steps: usize,
    pub message_count: usize,
    /// Top keywords extracted from the transcript (top 20 by frequency)
    pub keywords: Vec<String>,
}
```

### 2. Keyword extraction (on session close)

Simple TF-based extraction:
1. Concatenate all user + assistant messages in the session
2. Tokenize (split on whitespace + punctuation, lowercase)
3. Remove stop words
4. Count token frequency
5. Take top 20 tokens as `keywords`

### 3. New tool: `session_search`

```rust
pub struct SessionSearchTool {
    workspace: PathBuf,
}

// Tool parameters:
// - query: String (search terms)
// - days_back: Option<u32> (default: 30, max: 365)
// - limit: Option<usize> (default: 5)
//
// Returns: list of matching sessions with relevant snippets
```

Search logic:
1. Load `index.jsonl`
2. Filter by time range (`days_back`)
3. Score each session by keyword overlap with query
4. For top matches, load the actual `.jsonl` transcript
5. Find the 2-3 most relevant messages (those containing query terms)
6. Return formatted results

### 4. Tool output format

```json
{
  "matches": [
    {
      "session_id": "abc123",
      "goal": "Implement HTTP server",
      "date": "2026-05-26",
      "relevance": 0.85,
      "snippets": [
        "[user] How should we handle CORS?",
        "[assistant] I recommend using tower-http's CorsLayer with permissive defaults for development..."
      ]
    }
  ],
  "total_sessions_searched": 12
}
```

### 5. Snippet extraction

When loading a matching session's transcript:
- Find messages containing query terms
- Extract a window: the matching message + 1 message before and after
- Truncate each snippet to 200 chars
- Return max 3 snippets per session

### 6. Tests

- **Test A**: Index is created on session finish
- **Test B**: Keyword extraction produces reasonable terms
- **Test C**: `session_search` finds session by goal keywords
- **Test D**: `session_search` with `days_back` filter
- **Test E**: Snippet extraction includes relevant context
- **Test F**: Empty index returns empty results (no error)
- **Test G**: Large index (100+ sessions) searches efficiently

## Acceptance

- `cargo build` green.
- `cargo test` green (7+ new tests).
- `cargo clippy --all-targets -- -D warnings` green.
- After running 3+ sessions with `--session`, `session_search("http")`
  finds the one about HTTP if one exists.
- Search completes in < 100ms for 100 sessions.

## Notes for the agent

- Index is append-only JSONL (one line per session). Rebuild by
  scanning all meta.json files if index is missing/corrupt.
- DON'T load full transcripts during index search — only load for
  snippet extraction of top matches.
- Keyword extraction runs synchronously at session close. It's fast
  (just string counting) and doesn't need LLM.
- The `summary` field in index is optional. Simple approach: concatenate
  the goal + first assistant message (truncated to 200 chars).
- Future enhancement (NOT this goal): LLM-generated summaries, vector
  embeddings. Keep it keyword-based for now.
