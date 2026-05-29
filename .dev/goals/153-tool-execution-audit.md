# Goal 153 — Tool execution audit fields in `TranscriptEntry`

> **Roadmap**: Phase 18.5 — Long-running goals (part 1: durable execution journal)
> **Design principle check**:
> - **Orthogonal**: every tool dispatch produces an `AuditMeta`
>   alongside its result; one new persistence sink (g152's
>   `MessageAppended`) carries it onto disk. No globals, no
>   shared mutable slots.
> - **No agent loop / kernel changes**. The kernel still calls
>   tools the same way; this goal hooks the *registry dispatch* layer
>   that wraps every tool call already.
> - **No new persistence file**. Audit data lives in the transcript
>   we already write; the persistence vs LLM-wire boundary
>   established by `TranscriptEntry` ↔ `Message` (see g151) keeps
>   audit fields out of provider requests automatically.
> - **No tool API breakage**. `Tool::side_effect_class` is added with
>   a conservative default (`External`); existing tools opt in to
>   safer classes individually. The existing `Tool::is_readonly()`
>   method is **kept** and reimplemented as a default that delegates
>   to `side_effect_class()`.
> - **Depends on**: g151 (resume by ID, where orphan handling is
>   wired) and g152 (incremental transcript writes, without which
>   the orphan shape never lands on disk and detection has no input).

## Why

After g141 + g142, Recursive can checkpoint **workspace files** at
turn boundaries and rewind to any past turn. After g151 + g152,
`recursive resume` can re-open a JSONL session by ID and that
session's transcript reflects the agent's progress through the
last live turn (because g152 routes every committed message
through the persistence sink immediately).

What Recursive still cannot do is tell, after a crash mid-turn,
**whether a tool call's side-effect actually happened**. The
transcript records what the LLM intended (`assistant: tool_call
run_shell rm -rf foo/`) and any tool result that completed before
the crash; it does **not** annotate any of those with "this
mutated external state" vs "this only read state". On resume,
the LLM has no way to tell either, so the only safe option today
is to start over — which defeats the point of crash recovery.

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

### What g152 already gives us, and what's still missing

g152's `AgentEvent::MessageAppended` ensures that on resume we'll
see a transcript stopped at one of three shapes:

1. **Clean turn boundary** — last message is a `tool` reply for
   every preceding `tool_call`. Nothing to do.
2. **Orphan tool call** — last `assistant` message has tool calls,
   but one or more matching `tool` replies are missing (the process
   died inside `tools.invoke()` or just after, before the persistence
   sink saw the reply).
3. **Truncated last line** — extremely rare; the process died
   mid-write. `SessionReader::load_transcript` already skips
   un-parseable lines (`src/session.rs:534`).

Shape #2 is the **detection target** of this goal, and it is now
observable on disk. Shape #1 needs no work. Shape #3 is handled
by the loader.

Without g153, an orphan looks identical to a clean turn boundary
to the LLM (it just sees an assistant message with no tool reply
and assumes it should "respond" without context). With g153, the
runtime sees the orphan first, classifies its tools, and decides
how to proceed.

## Scope (do exactly this, no more)

### What this goal does

1. Define a **tool side-effect classification**
   `ToolSideEffect` (enum: `ReadOnly | Mutating | External`).
   The name is **`ToolSideEffect`**, not `SideEffect`, because
   `crate::kernel::SideEffect` already exists with a different
   meaning (background-job tracking — see `src/kernel.rs:121`).
   Using a distinct name avoids forcing a kernel-side rename.
2. Add `Tool::side_effect_class() -> ToolSideEffect` trait method,
   default `External`. Make `Tool::is_readonly()` (already in the
   trait, `src/tools/mod.rs:71`) **delegate** to the new method by
   default:
   ```rust
   fn is_readonly(&self) -> bool {
       matches!(self.side_effect_class(), ToolSideEffect::ReadOnly)
   }
   ```
   Existing built-in tools currently override `is_readonly` directly;
   migrate each one to override `side_effect_class` instead and let
   the default `is_readonly` fall through. Behaviour is preserved
   because `is_readonly` consumers (`agent.rs:467,469,995,998` —
   parallel-tool dispatch) still get the same `bool`.
3. Annotate every existing built-in tool with its actual class
   (table below).
4. Map MCP tool annotations (`readOnlyHint`, `destructiveHint`,
   `idempotentHint`, `openWorldHint`) into `ToolSideEffect` for MCP
   tools, gated by per-server trust config.
5. Add `audit: Option<AuditMeta>` to `TranscriptEntry`. Populated
   by the runtime from the audit value the registry returned.
6. Change `ToolRegistry::invoke` to **return audit alongside the
   result** (no shared mutable slot — see "Audit returned, not
   stashed" below).
7. Resume-side detection: a new
   `SessionReader::scan_orphan_tool_calls` helper that returns
   "tool_calls in the last assistant message that don't have a
   matching tool message".
8. Wire this into `recursive resume` (added in g151): if orphans
   exist, print a clear summary and refuse to proceed unless the
   user picks `--orphans=skip|redo|ask` (default `ask` on TTY,
   `abort` non-TTY).

### What this goal does **not** do

- Does not implement automatic safe replay (skip ReadOnly, redo
  Mutating, etc.) — that's g154's mutating-tool wrapper layer.
  This goal stops at *detection + user choice*.
- Does not garbage-collect any data. Audit fields stay in
  `transcript.jsonl` for the life of the session record. Their
  on-disk cost is bounded (~150 bytes per tool call, see "On-disk
  cost" below).
- Does not change MCP server behaviour. We only *read* annotations
  the spec already defines.
- Does not introduce a separate `tool_journal.jsonl`. That was an
  earlier design draft; consolidating into transcript is simpler
  and avoids a two-file consistency problem.
- Does not delete `Tool::is_readonly()`. It stays as a derived
  convenience for `agent.rs`'s parallel dispatch logic.

## Architecture

### `ToolSideEffect`

```rust
// New: src/tools/side_effect.rs (or in src/tools/mod.rs)

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSideEffect {
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

Naming note: a `SideEffect` enum already exists in
`src/kernel.rs:121` for background-job / scheduled-wakeup
tracking. `ToolSideEffect` is the **new** type; the kernel one is
unchanged. The two never appear in the same module.

### `Tool` trait

```rust
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn execute(&self, args: Value) -> Result<String>;

    /// Classify this tool's side-effect domain. Default is the most
    /// conservative (`External`) so that any tool not yet annotated
    /// is treated as risky on resume.
    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::External
    }

    /// Convenience: a tool is read-only iff it classifies as
    /// `ReadOnly`. Used by the parallel-dispatch path in
    /// `agent.rs`. Override only if you have an unusual reason
    /// (you almost never should — override `side_effect_class`
    /// instead and let this default through).
    fn is_readonly(&self) -> bool {
        matches!(self.side_effect_class(), ToolSideEffect::ReadOnly)
    }
}
```

Migration of existing tools: each built-in tool that overrides
`is_readonly()` today is rewritten to override `side_effect_class()`
instead. The `is_readonly()` override is removed unless the tool
intentionally diverges (none currently do).

Built-in tool classes (full list; one annotation per tool):

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
| `run_shell`, `run_skill_script`, `run_background` | External |
| `sub_agent` | External |
| `checkpoint_list`, `checkpoint_diff` | ReadOnly |
| `schedule_wakeup` | External |

\*`web_fetch` is technically external but our impl is GET-only with
no observable side-effect on remote state in practice. We classify
it ReadOnly with a code comment; if anyone adds POST support the
classification has to be revisited.

### MCP tool classification

```rust
// In McpTool::side_effect_class

let Some(ann) = &self.spec.annotations else {
    return ToolSideEffect::External;  // missing annotations → conservative
};

let trusted = self.server_trust == TrustLevel::Trusted;
if !trusted {
    return ToolSideEffect::External;  // untrusted server → don't believe hints
}

if ann.read_only {
    ToolSideEffect::ReadOnly
} else if ann.open_world {
    ToolSideEffect::External
} else {
    ToolSideEffect::Mutating
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
    /// Unique step id, formatted as a UUIDv7 hex string. UUIDv7 has
    /// a millisecond-precision timestamp prefix, so step IDs are
    /// time-ordered without needing a separate ULID dependency
    /// (we don't have `ulid` in tree; we already use UUID v4 in
    /// `agui-protocol`, so v7 is a one-feature-flag cost).
    pub step_id: String,
    /// Wall-clock millis when registry began the call.
    pub started_at: i64,
    /// Wall-clock millis when registry received the result.
    /// Always present in this goal: audit is built **after** the
    /// tool's `execute()` returns, before the registry returns the
    /// audit value to its caller. (The earlier draft had this as
    /// `Option<i64>` to leave room for a "start-time row" feature
    /// — that's deferred to a later goal; once it lands, this
    /// field will need to become `Option` *and* a new
    /// `audit_kind: started | finished` discriminator added.)
    pub finished_at: i64,
    /// blake3 of canonical-JSON(args). Detects argument drift across
    /// resumes (prompt/model changes producing different args).
    pub args_hash: String,
    /// As reported by the tool at registry-dispatch time. For MCP
    /// tools this is derived from annotations + trust at call time
    /// (so a config change later doesn't retroactively re-classify
    /// past calls).
    pub side_effect: ToolSideEffect,
    /// Ok / Err(message). Error message is **truncated** so a
    /// failed `apply_patch` carrying a multi-KB diff doesn't blow
    /// the audit row's size budget. See `AUDIT_ERR_MAX_BYTES`.
    pub exit_status: ExitStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExitStatus {
    Ok,
    Err {
        /// Truncated to AUDIT_ERR_MAX_BYTES (default 512 bytes,
        /// see "On-disk cost" below). If the original message was
        /// longer, `truncated: true` is set so consumers can tell.
        message: String,
        #[serde(default, skip_serializing_if = "is_false")]
        truncated: bool,
    },
}

/// Maximum length of the persisted error message in `ExitStatus::Err`.
/// Anything longer is suffix-clipped at a UTF-8 char boundary
/// (use `floor_char_boundary` or simple `chars().take(...)`)
/// and `truncated` is set.
pub const AUDIT_ERR_MAX_BYTES: usize = 512;

fn is_false(b: &bool) -> bool { !b }

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
millis there is fine for this goal.

### On-disk cost

A typical audit row, JSON-encoded:

```json
{
  "step_id": "0193c5a6-7f01-7e0c-9c12-1a8f0bff3d4d",
  "started_at": 1748563921003,
  "finished_at": 1748563921108,
  "args_hash": "a7f9c2…64 hex chars…",
  "side_effect": "read_only",
  "exit_status": {"type": "ok"}
}
```

That's ~190 bytes. On error with full 512-byte message it's
≤ 750 bytes. A 1000-call session is therefore bounded at
~750 KB even in the worst case (every call failing with maximum
payload). Acceptable; revisit if real workloads stretch this.

### UUID and blake3 dependencies

`uuid = { version = "1", features = ["v4"], optional = true }` is
already in `Cargo.toml`. g153 adds `"v7"` to its feature list and
makes it non-optional (or, if the `optional` flag is preserved
behind a feature gate elsewhere, ensures the audit code compiles
under all currently-enabled feature combos — verify before merging).

`blake3 = "1"` is already a direct dep — used as-is for `args_hash`.

No new external crate beyond a feature flag tweak.

### Audit returned, not stashed

The earlier sketch parked `AuditMeta` on a shared
`Arc<Mutex<Option<...>>>` slot owned by the registry, then had the
runtime drain it after each `invoke()`. **This is rejected.** Two
problems forced the redesign:

1. `ToolRegistry` is `Clone` and shared
   (`src/tools/mod.rs:76`). A shared mutable slot makes nested or
   parallel dispatch races easy to introduce silently — for example,
   `sub_agent` invokes `ToolRegistry` from inside another tool's
   `execute()`, which would clobber the parent's slot mid-dispatch.
2. The slot is global state for what is fundamentally a
   per-call value. The cleaner Rust idiom is to return it.

The new `invoke` signature returns audit alongside the result:

```rust
// src/tools/mod.rs

pub struct ToolDispatch {
    pub result: Result<String>,
    pub audit: AuditMeta,
}

impl ToolRegistry {
    /// Existing `invoke` keeps its return type for back-compat
    /// callers that don't care about audit (it just discards the
    /// audit half).
    pub async fn invoke(&self, name: &str, args: Value) -> Result<String> {
        self.invoke_with_audit(name, args).await.result
    }

    /// New: returns both result and audit.
    pub async fn invoke_with_audit(
        &self,
        name: &str,
        arguments: Value,
    ) -> ToolDispatch {
        // Static permission check before any tool execution
        // (existing code, unchanged).
        // ...

        let tool = match self.get(name) {
            Some(t) => t,
            None => return ToolDispatch {
                result: Err(Error::UnknownTool(name.into())),
                audit: AuditMeta::synthetic_unknown_tool(name),
            },
        };

        let side_effect = tool.side_effect_class();
        let step_id = uuid::Uuid::now_v7().hyphenated().to_string();
        let args_hash = blake3_canonical_json(&arguments);
        let started_at = unix_millis();

        let result = tool.execute(arguments).await;
        let finished_at = unix_millis();

        let exit_status = match &result {
            Ok(_) => ExitStatus::Ok,
            Err(e) => {
                let raw = e.to_string();
                let (clipped, truncated) = truncate_for_audit(&raw);
                ExitStatus::Err { message: clipped, truncated }
            }
        };

        ToolDispatch {
            result,
            audit: AuditMeta {
                step_id,
                started_at,
                finished_at,
                args_hash,
                side_effect,
                exit_status,
            },
        }
    }
}
```

`truncate_for_audit` clips at `AUDIT_ERR_MAX_BYTES` on a UTF-8
boundary and returns `(clipped_string, was_truncated)`.

### Wiring: how audit reaches `TranscriptEntry`

Today, `RunCore` (`src/agent.rs`) calls `tools.invoke(...)` and
uses the returned string to build `Message::tool_result(...)`,
which then goes through `push_message`. Goal 152 added an
`AgentEvent::MessageAppended { message: Message }` event fired at
every `push_message`, with `SessionPersistenceSink` writing each
message to disk.

g153 threads audit through that same seam:

1. Change `RunCore`'s tool-result path to call `invoke_with_audit`
   instead of `invoke`, capturing both the string and the audit.
2. Carry the audit alongside the constructed `Message` until it
   reaches `push_message`. The cleanest way is to add a new event
   variant **`AgentEvent::MessageAppendedWithAudit { message,
   audit }`** dedicated to tool messages, leaving the existing
   `MessageAppended` for everything else; the persistence sink
   handles both. (Don't bolt audit onto `Message` itself —
   provider adapters serialise `Message` to wire JSON and must
   not see audit.)
3. `SessionPersistenceSink` already takes `Arc<Mutex<SessionWriter>>`.
   Add a new `SessionWriter::append_with_audit(msg, audit)`
   variant that serialises a `TranscriptEntry` with the `audit`
   field populated. The plain `append(msg)` keeps its existing
   shape for non-tool messages.

```rust
// src/session.rs
impl SessionWriter {
    pub fn append(&mut self, msg: &Message) -> std::io::Result<String> {
        self.append_with_audit(msg, None)
    }

    pub fn append_with_audit(
        &mut self,
        msg: &Message,
        audit: Option<AuditMeta>,
    ) -> std::io::Result<String> {
        // ...existing append body, plus `entry.audit = audit;`
    }
}
```

The old `last_audit` slot does not exist. The registry never holds
shared mutable state.

### Resume-side detection

```rust
// In src/session.rs

pub struct OrphanToolCall {
    pub assistant_msg_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub args_hash: String,
    pub side_effect_at_call: ToolSideEffect,  // class at the time of dispatch
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
its `AuditMeta`. There is a non-obvious safety property here that
g151's tool-registry-hash check guarantees:

- `recursive resume <id>` (g151) refuses to proceed if the
  current `ToolRegistry` hash differs from the one stamped at
  session creation (`SessionMeta.tool_registry_hash`).
- Therefore, when orphan detection runs, we know the tool
  inventory and their classification logic is **identical** to
  what dispatched the original call. Looking up the class by
  current tool name is sound.
- The only way `side_effect_at_call` could be wrong is if a tool's
  `side_effect_class()` is non-deterministic (depends on per-call
  args), which no current tool does and the API does not
  encourage.

A "start-time audit row" feature (deferred) would let us record
the class at the actual moment of dispatch, removing this
indirection entirely. For now, the registry-hash gate is
sufficient.

### Resume CLI integration (extends g151)

```
recursive resume <id>                  # default: see policy below
recursive resume <id> --orphans=ask    # interactive (default if TTY)
recursive resume <id> --orphans=skip   # treat as completed
recursive resume <id> --orphans=redo   # re-execute (safe only for
                                       # ReadOnly; warns otherwise)
recursive resume <id> --orphans=abort  # exit non-zero (default if non-TTY)
```

Default policy when `--orphans` is not given:

- TTY (`std::io::stdin().is_terminal()` is `true`) → `ask`.
- Non-TTY (CI, SDK calls) → `abort` with exit code 2. Never
  silently re-execute External work. Never silently skip
  Mutating/External work.

When `--orphans=ask` and orphans exist, print a clear summary and
read a single character on stdin:

```
Session <id> has 1 incomplete tool call:

  step run_shell  (UUID 0193c5a6-7f01-7e0c-9c12-1a8f0bff3d4d)
    args: cargo test --release
    side-effect class: External
    started 2026-05-29T10:32:01Z, no finish recorded

This was likely interrupted by a crash. Choosing here:
  [r]edo  — re-run the command (UNSAFE for External tools)
  [s]kip  — assume it completed; proceed to next turn
  [a]bort — exit without resuming

Choice (r/s/a):
```

### Prompt UX: stdin only, no new dependency

The interactive prompt is a one-shot single-char read; we do **not**
add `dialoguer` / `inquire` / similar. Implementation:

```rust
fn prompt_orphan_choice() -> std::io::Result<OrphanChoice> {
    use std::io::{stdin, stdout, Write};
    let mut line = String::new();
    print!("Choice (r/s/a): ");
    stdout().flush()?;
    stdin().read_line(&mut line)?;
    match line.trim().to_ascii_lowercase().as_str() {
        "r" | "redo" => Ok(OrphanChoice::Redo),
        "s" | "skip" => Ok(OrphanChoice::Skip),
        "a" | "abort" | "" => Ok(OrphanChoice::Abort),
        other => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("unrecognised choice: {other:?}"),
        )),
    }
}
```

Empty line (just Enter) maps to `abort` — most conservative
default. Unrecognised input → re-prompt up to 3 times, then abort.
This is intentionally minimal; if richer UX is needed later we
can revisit, but a one-time crash-recovery prompt is not where to
spend a dependency.

For multiple orphans, prompt once per orphan in order. If all
should share an answer, the user can `Ctrl-C` and pass
`--orphans=skip` (or whichever) explicitly.

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
- `invoke_with_audit_returns_both_halves` — invoke a fake ReadOnly
  tool, assert the returned `ToolDispatch` has the expected
  `result` and an `audit.side_effect == ReadOnly` /
  `exit_status == Ok`.
- `invoke_with_audit_truncates_long_errors` — invoke a fake tool
  whose `execute()` returns a 4 KB error string, assert
  `ExitStatus::Err.message.len() <= AUDIT_ERR_MAX_BYTES` and
  `truncated == true`.
- `invoke_with_audit_clips_at_utf8_boundary` — error contains a
  multibyte char straddling the limit; assert clip preserves
  valid UTF-8.
- `is_readonly_default_delegates_to_class` — a tool that overrides
  only `side_effect_class` (not `is_readonly`) reports
  `is_readonly() == true` for `ReadOnly` and `false` for
  `Mutating`/`External`.
- `existing_is_readonly_overrides_migrated` — pick one built-in
  tool currently overriding `is_readonly`, after migration
  `agent.rs`'s parallel-dispatch path sees the same boolean
  outcome.

Integration (`tests/audit_e2e.rs`, new):

- `single_turn_audit_recorded` — run an agent turn with one
  ReadOnly tool, transcript entry has audit.side_effect == ReadOnly,
  exit_status == Ok.
- `failed_tool_records_err` — tool returns Err, audit has
  `ExitStatus::Err` with truncated message.
- `audit_persisted_via_g152_sink` — kill an agent run mid-turn
  (using the same hook as g152's `kill_mid_turn_persists_messages_so_far`
  test); reload the session and assert any tool messages that
  did land carry their audit fields, end-to-end.
- `orphan_detection_finds_killed_tool` — manually craft a session
  dir where the last assistant has a tool_call but no following
  tool message; `scan_orphan_tool_calls` returns it.
- `orphan_detection_returns_empty_on_clean_session` — every
  tool_call has a matching tool message → empty vec.
- `resume_aborts_on_orphans_default_non_tty` — session with
  orphans, plain `resume` from non-TTY → exit code 2, message
  names the orphan.
- `resume_skip_proceeds` — same session with `--orphans=skip` →
  appends a synthesised tool message saying `[skipped on resume]`,
  continues normally.
- `resume_ask_blank_line_aborts` — `--orphans=ask` from a piped
  stdin with a blank line → abort.
- `resume_ask_garbage_then_abort` — three rounds of `xxx\n`
  stdin → re-prompts then aborts (test doesn't deadlock).

## Acceptance

- `cargo build` green
- `cargo test` green; ≥ 17 new tests (10 unit + 7 integration)
- `cargo clippy --all-targets -- -D warnings` clean
- `cargo fmt --all -- --check` clean
- No new external crate. (`uuid` gains `v7` feature; `blake3`
  already direct; `dialoguer`-style prompt libs are NOT added.)
- Manual smoke: kill `recursive run "..."` mid-shell-call (ctrl-Z
  + `kill -9 %1`), then `recursive resume` shows the orphan and
  refuses without `--orphans=skip|redo`. Inspecting
  `transcript.jsonl` shows a tool entry with full `audit` block on
  every successfully-completed tool call.

## Out of scope (deferred)

- **Safe automatic replay** of orphans by class (skip ReadOnly,
  redo Mutating with verification, ask for External). That logic is
  g154 — needs a wrapper layer per tool class. This goal stops at
  detection + user-driven decision.
- **Audit at start time as well as finish**. Today we only emit
  `AuditMeta` post-execution; if the process dies *during* the
  call, the audit row never reaches the persistence sink (orphan
  detection works off the transcript shape instead). Recording a
  placeholder entry on start and updating on finish would let us
  record `started_at` for orphans and tighten
  `side_effect_at_call`, but doubles writes per tool call and
  needs an `audit_kind: started | finished` discriminator. Defer
  until we have evidence the precision is needed.
- **Garbage collection of audit data**. Audit rows live in
  `transcript.jsonl` for the life of the session record. At
  ≤ 750 bytes per tool call (worst-case error), a 1000-call
  session is bounded at ≤ 750 KB. Acceptable until proven
  otherwise.
- **Cross-version replay safety** (model upgraded, prompt changed,
  tool schema drifted between original run and resume). `args_hash`
  detects drift but we don't act on it yet; that's a richer policy
  question for g154 / 18.5b.
- **Surfacing audit data in `sessions show`** (a nicer UI for
  debugging). Trivial follow-up; not blocking.
- **Per-orphan batched answers** (`--orphans=ask` reading one
  choice that applies to all detected orphans). The current loop
  prompts per orphan; if a session has many orphans the user can
  Ctrl-C and re-invoke with the right flag. Revisit if real
  workloads need it.

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
