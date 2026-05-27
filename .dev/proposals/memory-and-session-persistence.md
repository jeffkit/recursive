# Proposal: Recursive Memory & Session Persistence System

> **Status**: Draft — pending review
> **Created**: 2026-05-27
> **Baseline**: v0.5.0
> **Scope**: Phase 14.1-14.3 (persistence) + Phase 14.5 (memory, new)

---

## Part 1: Session Persistence (JSONL)

### Current State

- `SessionFile` in `src/session.rs` dumps the entire transcript as a single JSON blob
- Only saves on abnormal exit (budget exceeded, stuck, transcript limit)
- HTTP server sessions live in `HashMap<String, SessionState>` — restart = lost
- All sessions dump into a flat `.recursive/sessions/` directory

### Target: Append-only JSONL with Workspace Scoping

#### Storage Layout

```
~/.recursive/
  sessions/
    <workspace-hash>/              # per-workspace isolation
      <session-id>/
        meta.json                  # session metadata (created, model, goal, status)
        transcript.jsonl           # append-only message log
      <session-id>/
        ...
    <workspace-hash>/
      ...
  memory/                          # global user memory (see Part 2)
    user.md
    facts.jsonl

<workspace>/.recursive/
  memory/                          # project-scoped memory (see Part 2)
    project.md
    facts.jsonl
```

**Workspace hash**: BLAKE3 of the canonical workspace path, truncated to 12 hex chars. A symlink `~/.recursive/sessions/<hash>/.workspace` points back to the original path for human discoverability.

#### JSONL Format (one message per line)

```jsonl
{"v":1,"ts":"2026-05-27T10:00:00.123Z","id":"msg_001","role":"system","content":"You are..."}
{"v":1,"ts":"2026-05-27T10:00:00.456Z","id":"msg_002","role":"user","content":"fix the bug"}
{"v":1,"ts":"2026-05-27T10:00:01.789Z","id":"msg_003","parent":"msg_002","role":"assistant","content":"Let me look...","tool_calls":[...]}
{"v":1,"ts":"2026-05-27T10:00:02.100Z","id":"msg_004","parent":"msg_003","role":"tool","tool_call_id":"call_001","content":"fn main()..."}
```

Fields:
- `v`: schema version (allows future migration)
- `ts`: ISO 8601 with milliseconds
- `id`: unique message ID (UUID or sequential)
- `parent`: optional, enables branching (retries, plan mode alternatives)
- `role`: system / user / assistant / tool
- `content`, `tool_calls`, `tool_call_id`: standard message fields
- Future: `usage`, `latency_ms`, `model` per-message (optional, for observability)

#### Write Strategy

```rust
pub struct SessionWriter {
    file: BufWriter<File>,  // opened in append mode
    path: PathBuf,
}

impl SessionWriter {
    /// Append a single message. Flush after each write for crash safety.
    pub fn append(&mut self, msg: &Message) -> io::Result<()> {
        let line = serde_json::to_string(&msg)?;
        writeln!(self.file, "{}", line)?;
        self.file.flush()  // fsync optional, flush sufficient for most cases
    }
}
```

#### Read Strategy

```rust
pub fn load_transcript(path: &Path) -> io::Result<Vec<Message>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    reader.lines()
        .filter_map(|line| line.ok())
        .filter_map(|line| serde_json::from_str(&line).ok())  // skip malformed lines
        .collect()
}
```

#### Resume Flow

1. `recursive resume <session-id>` or `recursive resume --last`
2. Load `meta.json` → validate tool registry hash
3. Stream `transcript.jsonl` → rebuild `Vec<Message>`
4. Inject into Agent, continue from last state

#### CLI Integration

```bash
recursive sessions list                    # list all sessions for current workspace
recursive sessions list --all              # list across all workspaces
recursive sessions show <id>               # pretty-print transcript
recursive sessions resume <id> [goal]      # resume from session
recursive sessions gc --older-than 30d     # garbage collect old sessions
```

---

## Part 2: Memory System

### Research Survey: SOTA Agent Memory (2025-2026)

| Framework | Architecture | Key Innovation | Trade-off |
|-----------|-------------|----------------|-----------|
| **Claude Code** | File-based hierarchy (CLAUDE.md at enterprise/project/user/directory levels) | Git-friendly, transparent, zero-infra | No semantic search, manual curation |
| **Letta (MemGPT)** | OS-inspired 3-tier (Core/Recall/Archival) | Agent self-edits memory like OS paging | High storage cost (~10GB/agent at scale) |
| **Mem0** | 5-layer hierarchy + dual storage (vector + graph DB) | Automatic extraction + scoring + forgetting | Operational complexity (2+ LLM calls per op) |
| **Hermes Agent** | 3-layer (MEMORY.md + External Providers + SQLite FTS) | Freeze-snapshot for cache stability; self-evolving SKILL.md | Provider lock-in for Layer 2 |
| **OpenHands** | Event-sourced + Condenser modules | Dynamic compression, context offloading | Tight coupling to event model |

### Design Principles for Recursive

Drawing from the best of each:

1. **File-first** (from Claude Code + Hermes): Memory is Markdown/JSONL files, human-readable, Git-friendly
2. **Agent self-manages** (from Letta): Agent has tools to read/write memory, not just passive injection
3. **Layered with clear boundaries** (from Mem0): Each layer has a distinct purpose and storage
4. **Freeze-snapshot** (from Hermes): Memory loaded at session start, updates persist for next session
5. **No external infra required** (from our principles): No vector DB, no graph DB for core. Optional for extensions.

### Memory Architecture: 4 Layers

```
┌─────────────────────────────────────────────────────────────────────────┐
│ Layer 0: Injected Context (read-only at session start)                  │
│                                                                         │
│  Sources:                                                               │
│   - AGENTS.md (workspace root) ← already implemented                   │
│   - ~/.recursive/memory/user.md (user-global preferences)              │
│   - <workspace>/.recursive/memory/project.md (project context)         │
│   - Skills' context files (already implemented)                         │
│                                                                         │
│  Behavior: Concatenated into system prompt. Frozen for session.         │
│  Size cap: 16KB per source (existing AGENTS.md cap)                     │
└─────────────────────────────────────────────────────────────────────────┘
         ▼ agent can read via tool
┌─────────────────────────────────────────────────────────────────────────┐
│ Layer 1: Working Memory (read-write during session)                     │
│                                                                         │
│  Storage: <workspace>/.recursive/memory/scratchpad.json                 │
│  Contents: Structured key-value pairs for current task state            │
│  Tools: memory_get(key), memory_set(key, value), memory_list()          │
│                                                                         │
│  Behavior: Persists to disk on every write. Available across sessions.  │
│  Use case: "Current task is X", "User prefers Y", "Blocked on Z"       │
│  Size cap: 64KB total                                                   │
└─────────────────────────────────────────────────────────────────────────┘
         ▼ agent can search via tool
┌─────────────────────────────────────────────────────────────────────────┐
│ Layer 2: Semantic Memory (long-term facts, cross-session)               │
│                                                                         │
│  Storage: <workspace>/.recursive/memory/facts.jsonl                     │
│           ~/.recursive/memory/facts.jsonl (global)                      │
│  Contents: Extracted facts with metadata                                │
│  Tools: remember(fact, tags[]), recall(query, limit?), forget(id)       │
│                                                                         │
│  Behavior: Append-only log. Agent extracts facts from conversations.    │
│  Search: FTS (full-text search) on text + tag filtering.                │
│  Dedup: On write, check similarity with recent N facts.                 │
│  Eviction: Staleness score = age × (1 - access_frequency)              │
│  Size cap: 1000 facts per scope (project/global), FIFO eviction beyond  │
└─────────────────────────────────────────────────────────────────────────┘
         ▼ agent can search via tool
┌─────────────────────────────────────────────────────────────────────────┐
│ Layer 3: Episodic Memory (historical session recall)                    │
│                                                                         │
│  Storage: ~/.recursive/sessions/<workspace-hash>/*/transcript.jsonl     │
│  Contents: Full historical transcripts                                  │
│  Tools: session_search(query, time_range?, limit?)                      │
│                                                                         │
│  Behavior: Read-only search over past sessions.                         │
│  Search: Keyword match over message content + session metadata.         │
│  Returns: Relevant message snippets with session context.               │
│  Future: FTS5 index for fast full-text search at scale.                 │
└─────────────────────────────────────────────────────────────────────────┘
```

### Layer 0 Detail: Injected Context Files

The **file discovery and loading order** at session start:

```
1. ~/.recursive/memory/user.md              (user global preferences)
2. <workspace>/AGENTS.md                    (project context — EXISTING)
3. <workspace>/.recursive/memory/project.md (project-specific memory)
4. Skill injection contexts                 (EXISTING)
```

All concatenated under headers in system prompt:

```
# User context
<content of user.md>

# Project context (AGENTS.md)
<content of AGENTS.md>

# Project memory
<content of project.md>

---
<default system prompt>
```

**Relation to AGENTS.md**: AGENTS.md is already loaded (goal #36, implemented). It serves as the "project context" layer — team-maintained, checked into Git. The new `user.md` adds a personal layer (like Claude Code's `~/.claude/CLAUDE.md`), and `project.md` adds an agent-writable project memory (like Claude Code's `.claude/CLAUDE.local.md`).

### Layer 1 Detail: Working Memory

Replaces the current `memory.json` (which stores flat notes) with a structured scratchpad:

```json
{
  "version": 1,
  "entries": {
    "current_task": {
      "value": "Implementing session persistence",
      "updated_at": "2026-05-27T10:30:00Z"
    },
    "user_preference_lang": {
      "value": "Rust, but open to Python for scripts",
      "updated_at": "2026-05-26T14:00:00Z"
    },
    "blocked_on": {
      "value": null,
      "updated_at": "2026-05-27T11:00:00Z"
    }
  }
}
```

Tools:
- `memory_get(key) → value | null`
- `memory_set(key, value)` — upserts, updates timestamp
- `memory_delete(key)` — sets value to null (tombstone)
- `memory_list() → [(key, value, updated_at)]`

### Layer 2 Detail: Semantic Memory (Facts)

Enhanced version of current `remember`/`recall`/`forget` tools:

```jsonl
{"id":"F001","text":"This project uses Tokio async runtime with multi-threaded scheduler","tags":["architecture","rust"],"source":"session_abc123","ts":"2026-05-25T14:00:00Z","access_count":3,"last_accessed":"2026-05-27T10:00:00Z"}
{"id":"F002","text":"User prefers JSONL over SQLite for persistence","tags":["preference","architecture"],"source":"session_def456","ts":"2026-05-27T10:30:00Z","access_count":0,"last_accessed":null}
```

**Automatic fact extraction** (post-session hook):
1. After session ends (or on compaction), summarize key decisions/facts
2. Compare with existing facts (fuzzy match on text similarity)
3. Append new, update duplicates, mark contradictions

**Search strategy** (no vector DB):
1. Keyword tokenization of query
2. Match against `text` + `tags` fields
3. Score = term_frequency × recency_boost × access_frequency_boost
4. Return top-K results

### Layer 3 Detail: Episodic Memory (Session Search)

Lightweight session index for cross-session recall:

```json
// ~/.recursive/sessions/<workspace-hash>/index.json
{
  "sessions": [
    {
      "id": "abc123",
      "goal": "Implement HTTP server",
      "model": "claude-sonnet-4",
      "created_at": "2026-05-26T10:00:00Z",
      "finished_at": "2026-05-26T11:30:00Z",
      "steps": 24,
      "summary": "Built HTTP server with Axum, added session management and SSE streaming",
      "keywords": ["http", "axum", "sse", "session"]
    }
  ]
}
```

Tool: `session_search(query, days_back?, limit?)`
- Searches index by keyword/summary match
- If hit, reads relevant portions of `transcript.jsonl`
- Returns formatted snippets (not full transcripts — avoid context explosion)

---

## Part 3: Migration from Current Implementation

### What exists today

| Component | Location | Fate |
|-----------|----------|------|
| `SessionFile` (JSON dump) | `src/session.rs` | Replace with JSONL writer |
| `MemoryStore` (notes) | `src/tools/memory.rs` | Evolve into Layer 1 + Layer 2 |
| `load_project_context` (AGENTS.md) | `src/config.rs` | Keep as Layer 0 source |
| HTTP `HashMap<SessionState>` | `src/http.rs` | Back with JSONL persistence |

### Implementation Phases

```
Phase A — Session JSONL (replaces current session.rs)
  A.1: SessionWriter (append-only JSONL)
  A.2: Workspace-scoped directory structure
  A.3: SessionReader (load transcript from JSONL)
  A.4: CLI commands (list, show, resume, gc)
  A.5: Wire into Agent run loop (write per-message)
  A.6: Wire into HTTP server (persist sessions across restarts)

Phase B — Layer 0 Enhancement (Injected Context)
  B.1: Add ~/.recursive/memory/user.md loading
  B.2: Add <workspace>/.recursive/memory/project.md loading
  B.3: Unify loading order with existing AGENTS.md

Phase C — Layer 1 Working Memory (Scratchpad)
  C.1: Refactor memory.rs from flat notes to structured KV
  C.2: New tools: memory_get, memory_set, memory_delete, memory_list
  C.3: Auto-inject top-K recent entries into prompt (optional)

Phase D — Layer 2 Semantic Memory (Facts)
  D.1: facts.jsonl schema + read/write helpers
  D.2: Enhanced remember/recall/forget tools with tags + scoring
  D.3: FTS search implementation (pure Rust, no deps)
  D.4: Post-session fact extraction hook

Phase E — Layer 3 Episodic Memory (Session Search)
  E.1: Session index builder (writes index.json on session close)
  E.2: session_search tool implementation
  E.3: Snippet extraction from historical transcripts
```

---

## Part 4: Design Decisions & Rationale

### Why not SQLite?

| Concern | JSONL answer |
|---------|-------------|
| Concurrent writes | Append-only = no conflicts |
| Crash recovery | At most lose last line |
| Portability | Copy files, done |
| Debuggability | `cat`, `jq`, `grep` |
| Dependencies | Zero (vs. `rusqlite` + C lib) |
| Performance at scale | Fine for <100K messages per session |

**When to reconsider**: If we need cross-session JOINs (e.g., "total cost across all sessions this week"), or if session count exceeds ~10K per workspace. That's a v0.8+ concern.

### Why freeze-snapshot for memory (Hermes approach)?

Loading memory once at session start (instead of dynamically reloading on every turn) because:
1. **Prefix cache friendly** — system prompt stays stable, LLM can cache it
2. **Predictable** — no surprise context changes mid-conversation
3. **Cheaper** — no extra reads per turn
4. **Safe** — prevents runaway memory growth within a session

New facts written during a session are persisted to disk but only visible in the next session's prompt.

### Why no vector DB?

For Recursive's scale (individual developer tool, not SaaS platform):
- Sessions rarely exceed 10K messages total
- Facts rarely exceed 1000 per workspace
- Keyword FTS is sufficient and adds zero infrastructure
- Vector search can be added as Layer 2.5 later (optional `tantivy` or embedding-based index)

---

## References

### Implementations studied

- **Claude Code**: Hierarchical CLAUDE.md (enterprise → project → user → directory), JSONL conversations, auto-dream dedup
- **Letta/MemGPT**: Core/Recall/Archival tiers, agent self-edit via tools, OS paging metaphor
- **Mem0**: 5-layer (sensory → short → working → long → meta), dual vector+graph storage, automatic scoring
- **Hermes Agent**: MEMORY.md + USER.md (char-limited), freeze-snapshot, self-evolving SKILL.md, SQLite FTS5 for session search
- **OpenHands**: Event-sourced memory, Condenser modules for compression, RecallAction events

### Design influences on Recursive

| Borrowed from | What | Adapted how |
|---------------|------|-------------|
| Claude Code | File hierarchy, Git-friendly | user.md + project.md + AGENTS.md |
| Hermes | Freeze-snapshot, size caps | Load once at start, 16KB per source |
| Letta | Agent self-edit memory | memory_set/get/delete tools |
| Mem0 | Fact scoring + eviction | access_count + staleness score |
| OpenHands | Event compression | Future: compaction writes summary to facts |
