# Goal 153 — Tool execution audit fields in `TranscriptEntry`

> **Roadmap**: Phase 18.5 — Long-running goals (part 1: durable execution journal)
> **Design principle check**:
> - **Orthogonal**: writes happen in one place (`ToolRegistry::invoke`),
>   reads happen in one place (resume detection). Adds a single
>   nullable field to `TranscriptEntry`.
> - **No agent loop / kernel changes**. The kernel still calls
>   tools the same way; this goal hooks the *registry dispatch* layer
>   that wraps every tool call already.
> - **No new persistence file**. Audit data lives in the transcript
>   we already write; the persistence vs LLM-wire boundary
>   established by `TranscriptEntry` ↔ `Message` (see g151) keeps
>   audit fields out of provider requests automatically.
> - **No tool API breakage**. `Tool::side_effect_class` is added with
>   a conservative default (`External`); existing tools opt in to
>   safer classes individually.

## Why

After g141 + g142, Recursive can checkpoint **workspace files** at
turn boundaries and rewind to any past turn. What it still cannot do
is tell, after a crash mid-turn, **whether a tool call's
side-effect actually happened**. The transcript only records what the
LLM intended (`assistant: tool_call run_shell rm -rf foo/`); whether
the shell command ran, succeeded, or got killed before exit is
unknowable from the transcript alone. On resume, the LLM has no way
to tell either, so the only safe option today is to start over —
which defeats the point of crash recovery.

Concretely, for these scenarios there is no good answer in the
current design:

- Long task OOMs at file 47 of 100 — was patch 47 written?
- A `run_shell git push` exits 137 (sigkill) — did the push complete?
- `recursive resume <id>` after a power cut — does the agent re-do
  the last tool call (risk: duplicate side-effect) or skip it (risk:
  silent missing step)?

LangGraph's checkpointer family has the same gap by design:
"resuming from a checkpoint before a tool call re-executes that tool
call." Their guidance is "implement idempotency keys yourself." For
Recursive we want to give the runtime enough information to do this
without burdening every tool author.

## Scope (do exactly this, no more)

### What this goal does

1. Define a **side-effect classification** for tools (`SideEffect`
   enum: `ReadOnly | Mutating | External`).
2. Add `Tool::side_effect_class()` trait method, default `External`.
3. Annotate every existing built-in tool with its actual class.
4. Map MCP tool annotations (`readOnlyHint`, `destructiveHint`,
   `idempotentHint`, `openWorldHint`) into `SideEffect` for MCP
   tools, gated by per-server trust config.
5. Add `audit: Option<AuditMeta>` to `TranscriptEntry`. Populated
   automatically by the registry on every tool call.
6. Resume-side detection: a new `SessionReader::scan_orphan_tool_calls`
   helper that returns "tool_calls in the last assistant message
   that don't have a matching tool message".
7. Wire this into `recursive resume` (added in g151): if orphans
   exist, print a clear summary and refuse to proceed unless the
   user picks `--orphans=skip|redo|ask` (default `ask`).

### What this goal does **not** do

- Does not implement automatic safe replay (skip ReadOnly, redo
  Mutating, etc.) — that's g154's mutating-tool wrapper layer.
  This goal stops at *detection + user choice*.
- Does not garbage-collect any data. Audit fields stay in
  `transcript.jsonl` for the life of the session record. Their
  on-disk cost is ~100 bytes per tool call.
- Does not change MCP server behaviour. We only *read* annotations
  the spec already defines.
- Does not introduce a separate `tool_journal.jsonl`. That was an
  earlier design draft; consolidating into transcript is simpler
  and avoids a two-file consistency problem.

## Architecture

### `SideEffect`

```rust
// New: src/tools/side_effect.rs (or in src/tools/mod.rs)

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideEffect {
    /// No mutation of any state outside the agent. Safe to replay
    /// at any time. `read_file`, `list_dir`, `grep`, `recall`, etc.
    ReadOnly,
    /// Modifies local state (filesystem, scratchpad) but the change
    /// is contained, idempotent-friendly, and verifiable by reading
    /// state back. `write_file`, `apply_patch`, `remember`.
    Mutating,
    /// Reaches out to external world. Cannot determine safe
    /// re-execution from local state alone. `run_shell`, `web_fetch`,
    /// most MCP tools that aren't explicitly read-only.
    External,
}
```

### `Tool` trait

```rust
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn execute(&self, args: Value) -> Result<String>;

    /// Classify this tool's side-effect domain. Default is the most
    /// conservative (`External`) so that any tool not yet annotated
    /// is treated as risky on resume.
    fn side_effect_class(&self) -> SideEffect {
        SideEffect::External
    }
}
```

Built-in tools opt in (representative; full list in implementation):

| Tool | Class |
|---|---|
| `read_file` | ReadOnly |
| `list_dir` | ReadOnly |
| `search_files` (grep) | ReadOnly |
| `estimate_tokens` | ReadOnly |
| `recall_fact`, `episodic_recall`, `load_skill` | ReadOnly |
| `web_fetch` (GET only) | ReadOnly\* |
| `write_file`, `apply_patch` | Mutating |
| `remember_fact`, scratchpad write | Mutating |
| `run_shell`, `run_skill_script` | External |
| `sub_agent` | External |
| `checkpoint_list`, `checkpoint_diff` | ReadOnly |

\*`web_fetch` is technically external but our impl is GET-only with
no observable side-effect on remote state in practice. We classify
it ReadOnly with a code comment; if anyone adds POST support the
classification has to be revisited.

### MCP tool classification

```rust
// In McpTool::side_effect_class

let Some(ann) = &self.spec.annotations else {
    return SideEffect::External;  // missing annotations → conservative
};

let trusted = self.server_trust == TrustLevel::Trusted;
if !trusted {
    return SideEffect::External;  // untrusted server → don't believe hints
}

if ann.read_only {
    SideEffect::ReadOnly
} else if ann.open_world {
    SideEffect::External
} else {
    SideEffect::Mutating
}
```

`McpToolSpec` gains an optional `annotations: McpToolAnnotations`
field (already a 2025-03-26 spec field; we just deserialize it).
Per-server trust comes from a new config block:

```toml
# ~/.recursive/config.toml or .recursive/mcp.json
[mcp.servers.my-internal]
command = "internal-mcp-bin"
trust = "trusted"           # values: "trusted" | "untrusted" (default)
```

`idempotentHint` is **read but not used** in this goal — it's only
relevant for safe-replay decisions (g154).

### `AuditMeta` and `TranscriptEntry`

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditMeta {
    /// Unique step id, ULID. Stable across processes.
    pub step_id: String,
    /// Wall-clock millis when registry began the call.
    pub started_at: i64,
    /// Wall-clock millis when registry received the result. None
    /// only on the live, in-progress call (cleared before next entry
    /// is appended); a finished session never has None here.
    pub finished_at: Option<i64>,
    /// blake3 of canonical-JSON(args). Detects argument drift across
    /// resumes (prompt/model changes producing different args).
    pub args_hash: String,
    /// As reported by the tool at registry-dispatch time. For MCP
    /// tools this is derived from annotations + trust at call time
    /// (so a config change later doesn't retroactively re-classify
    /// past calls).
    pub side_effect: SideEffect,
    /// Ok / Err(message). Cheaper to read than parsing the full
    /// `content` field on resume.
    pub exit_status: ExitStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExitStatus {
    Ok,
    Err { message: String },
}

// Add to TranscriptEntry (in src/session.rs):
pub struct TranscriptEntry {
    // ...existing fields...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit: Option<AuditMeta>,
}
```

`audit` is set **only on `role: "tool"` entries** (the result
message). The `assistant` message that contains `tool_calls` does
not get audit data — start-time is implicitly "the moment between
that assistant message and the orphan check"; not having an exact
millis there is fine.

### Where audit gets written

```rust
// src/tools/mod.rs — ToolRegistry::invoke (sketch)

pub async fn invoke(&self, name: &str, args: Value) -> Result<String> {
    let tool = self.find(name).ok_or(...)?;
    let side_effect = tool.side_effect_class();
    let step_id = ulid::Ulid::new().to_string();
    let args_hash = blake3_canonical_json(&args);
    let started_at = chrono::Utc::now().timestamp_millis();

    let result = tool.execute(args).await;

    let finished_at = chrono::Utc::now().timestamp_millis();
    let audit = AuditMeta {
        step_id,
        started_at,
        finished_at: Some(finished_at),
        args_hash,
        side_effect,
        exit_status: match &result {
            Ok(_) => ExitStatus::Ok,
            Err(e) => ExitStatus::Err { message: e.to_string() },
        },
    };

    // Stash on a slot the runtime drains after the call returns,
    // so the runtime can attach `audit` to the tool result message
    // before SessionWriter::append serialises it. See "Wiring" below.
    self.last_audit.lock().await.replace(audit);

    result
}
```

### Wiring: how audit reaches `TranscriptEntry`

The runtime today (`src/runtime.rs`) loops:
1. Ask LLM → assistant message.
2. For each tool_call → registry invoke → build a `Message::tool(...)`
   reply.
3. Append both messages to the session writer.

We add a step 2.5: read the registry's `last_audit` slot after
invoke and attach it to the `Message` before append. `Message`
itself does **not** gain an audit field — only `TranscriptEntry`
does. The runtime owns the small adapter:

```rust
// In SessionWriter::append (or a new variant), allow caller to
// supply audit for tool messages:
pub fn append_with_audit(
    &mut self,
    msg: &Message,
    audit: Option<AuditMeta>,
) -> std::io::Result<String>;
```

`append` keeps its existing signature, calls
`append_with_audit(msg, None)`. Provider adapters already build wire
JSON by hand from `Message` fields and never touch `TranscriptEntry`,
so this stays cleanly separated.

### Resume-side detection

```rust
// In src/session.rs

pub struct OrphanToolCall {
    pub assistant_msg_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub args_hash: String,
    pub side_effect_at_call: SideEffect,  // class at the time of dispatch
}

impl SessionReader {
    /// Scan transcript backwards from the end. If the last
    /// assistant message has tool_calls but the trailing tail
    /// is missing one or more matching `tool` messages, return
    /// the missing ones. Empty vec = no orphans.
    pub fn scan_orphan_tool_calls(
        session_dir: &Path,
    ) -> std::io::Result<Vec<OrphanToolCall>>;
}
```

`side_effect_at_call` for an orphan is **derived from the current
tool registry**, since the call never finished and so never wrote
its `AuditMeta`. We could improve this later by writing audit at
*start* time too, but that needs a transcript-rewrite step; out of
scope here.

### Resume CLI integration (extends g151)

```
recursive resume <id>                  # default: error if orphans
recursive resume <id> --orphans=ask    # interactive (default if TTY)
recursive resume <id> --orphans=skip   # treat as completed
recursive resume <id> --orphans=redo   # re-execute (safe only for
                                       # ReadOnly; warns otherwise)
```

When `--orphans=ask` and orphans exist, print:

```
Session <id> has 1 incomplete tool call:

  step run_shell  (ULID 01J...)
    args: cargo test --release
    side-effect class: External
    started 2026-05-29T10:32:01Z, no finish recorded

This was likely interrupted by a crash. Choosing here:
  [r]edo  — re-run the command (UNSAFE for External tools)
  [s]kip  — assume it completed; proceed to next turn
  [a]bort — exit without resuming

Choice (r/s/a):
```

For non-TTY runs (CI, SDK calls), default is **abort with exit
code 2** unless `--orphans=skip|redo` is passed. Never silently
re-execute External work.

## Tests

Unit:

- `audit_round_trip` — write a `TranscriptEntry` with audit, reload,
  fields match.
- `audit_skipped_when_none` — entry without audit serialises without
  the field; old transcripts (no field) still parse.
- `mcp_classification_uses_annotations_when_trusted` — feed a tool
  spec with `readOnlyHint=true`, server trusted → ReadOnly.
- `mcp_classification_pessimistic_when_untrusted` — same spec,
  server untrusted → External regardless of hint.
- `mcp_classification_pessimistic_when_no_annotations` — no
  annotations field → External.
- `tool_registry_emits_audit` — invoke a fake ReadOnly tool, assert
  the registry's last_audit slot has the correct fields.

Integration (`tests/audit_e2e.rs`, new):

- `single_turn_audit_recorded` — run an agent turn with one
  ReadOnly tool, transcript entry has audit.side_effect == ReadOnly,
  exit_status == Ok.
- `failed_tool_records_err` — tool returns Err, audit has
  ExitStatus::Err with message.
- `orphan_detection_finds_killed_tool` — manually craft a session
  dir where the last assistant has a tool_call but no following
  tool message; `scan_orphan_tool_calls` returns it.
- `orphan_detection_returns_empty_on_clean_session` — every
  tool_call has a matching tool message → empty vec.
- `resume_aborts_on_orphans_default` — session with orphans, plain
  `resume` → exit code 2, message names the orphan.
- `resume_skip_proceeds` — same session with `--orphans=skip` →
  appends a synthesised tool message saying `[skipped on resume]`,
  continues normally.

## Acceptance

- `cargo build` green
- `cargo test` green; ≥ 12 new tests
- `cargo clippy --all-targets -- -D warnings` clean
- `cargo fmt --all -- --check` clean
- Manual smoke: kill `recursive run "..."` mid-shell-call (ctrl-Z
  + `kill -9 %1`), then `recursive resume` shows the orphan and
  refuses without `--orphans=skip|redo`.

## Out of scope (deferred)

- **Safe automatic replay** of orphans by class (skip ReadOnly,
  redo Mutating with verification, ask for External). That logic is
  g154 — needs a wrapper layer per tool class. This goal stops at
  detection + user-driven decision.
- **Audit at start time as well as finish**. Today we only write
  `AuditMeta` post-execution; if the process dies *during* the call,
  we have no audit row at all (orphan detection works off the
  transcript shape instead). Recording a placeholder entry on start
  and updating on finish would let us record `started_at` for
  orphans, but doubles writes per tool call. Defer until we have
  evidence the precision is needed.
- **Garbage collection of audit data**. The fields stay forever in
  `transcript.jsonl`. At ~100 bytes per tool call, a 1000-call
  session is 100KB. Acceptable until proven otherwise.
- **Cross-version replay safety** (model upgraded, prompt changed,
  tool schema drifted between original run and resume). `args_hash`
  detects drift but we don't act on it yet; that's a richer policy
  question for g154 / 18.5b.
- **Surfacing audit data in `sessions show`** (a nicer UI for
  debugging). Trivial follow-up; not blocking.

## Why this is the smallest useful step

This goal is deliberately the **detection** half of durable
execution, with no automation. The reason is simple: the moment we
start auto-replaying tool calls based on classification, we are
making safety guarantees we can't fully back up (MCP annotations
are hints, not contracts; `args_hash` detects drift but doesn't
explain it). That logic deserves its own goal with its own test
matrix. Detection alone is already a real product win — today the
agent can't tell you "by the way, your last run died with an
unfinished `git push`"; after this goal, it can.
