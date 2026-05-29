# Goal 154 — Class-aware safe replay of orphan tool calls

> **Roadmap**: Phase 18.5 — Long-running goals (part 2: safe replay)
> **Design principle check**:
> - Builds on g153's detection + classification. No new persistence,
>   no new public types except a per-tool-class policy struct.
> - Touches one runtime path (`run_resumed`'s pre-flight orphan
>   handler) and adds an opt-in `Tool::verify_completion` extension
>   for tools that can self-check.
> - Conservative defaults: any uncertainty → ask the user. Never
>   silently re-execute `External` work.
> - **Depends on**: g151 (resume by ID), g152 (incremental
>   transcript writes), g153 (audit fields + side-effect
>   classification).

## Why

g153 lets `recursive resume` *detect* orphan tool calls — calls
where the last assistant message scheduled work but no tool result
came back before the crash. It hands the user a binary choice
(skip / redo) regardless of the orphan's risk profile. That is the
right *first* step (no surprises), but it's not a great long-term
default — for a long-running task with hundreds of tool calls, even
one crash means the user has to manually answer prompts before
resuming.

Most orphans don't actually need a human. Concretely:

- A `ReadOnly` orphan (`grep`, `read_file`) can be re-run safely.
  Re-running costs at most one extra LLM token-budget worth of work.
- A `Mutating` orphan that targets a file (`apply_patch`,
  `write_file`) can in principle be **verified**: read the file
  back, compare its hash with what the args say it should now be.
  If the file already matches the post-call state, the patch
  succeeded; if it matches the pre-call state, it didn't. (A patch
  caught mid-write is the only ambiguous case, and our writes are
  small enough that this is rare.)
- An `External` orphan is genuinely uncertain. Default stays
  user-prompted.

This goal turns the binary skip/redo into a **policy** that knows
about side-effect class and lets each tool optionally self-verify.

## Scope

### Per-class default policy

```rust
pub struct ReplayPolicy {
    pub readonly: ClassAction,
    pub mutating: ClassAction,
    pub external: ClassAction,
}

#[derive(Debug, Clone, Copy)]
pub enum ClassAction {
    /// Re-execute the tool with the recorded args. Safe for ReadOnly,
    /// risky otherwise.
    Redo,
    /// Insert a synthetic tool result `[skipped on resume]` and
    /// move on. Safe when subsequent steps don't depend on the
    /// result, dangerous when they do.
    Skip,
    /// Try `Tool::verify_completion`; if it returns Done, treat as
    /// completed; if NotDone, redo; if Unknown, fall through to
    /// `Ask`.
    Verify,
    /// Stop and ask the user (TTY) or abort with exit 2 (non-TTY).
    Ask,
}

impl Default for ReplayPolicy {
    fn default() -> Self {
        Self {
            readonly: ClassAction::Redo,
            mutating: ClassAction::Verify,
            external: ClassAction::Ask,
        }
    }
}
```

User overrides the default via CLI flags:

```
recursive resume <id>                       # default policy
recursive resume <id> --replay=ask          # ask for everything
recursive resume <id> --replay=skip-mutating
recursive resume <id> --replay-external=skip   # power-user, dangerous
```

The g153 flags (`--orphans=skip|redo|ask`) become aliases that set
all three classes uniformly, kept for backwards compatibility.

### Optional `Tool::verify_completion`

```rust
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn execute(&self, args: Value) -> Result<String>;
    fn side_effect_class(&self) -> SideEffect { SideEffect::External }

    /// Best-effort: did this tool's side-effect already happen,
    /// based on the *current* state of the world?
    ///
    /// Called only on resume, only for orphan calls, only when the
    /// replay policy for this tool's class is `Verify`. The default
    /// returns `Unknown` — implementations override only when they
    /// can answer cheaply and reliably.
    async fn verify_completion(
        &self,
        _args: &Value,
    ) -> Result<CompletionStatus> {
        Ok(CompletionStatus::Unknown)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CompletionStatus {
    /// The side-effect is already in place. Synthesise a tool result
    /// reflecting the verified state, do not re-execute.
    Done { synthesised_result: String },
    /// The side-effect did not happen. Safe to re-execute.
    NotDone,
    /// Cannot tell. Caller falls back to `Ask`.
    Unknown,
}
```

### Implementations to ship in this goal

We do **not** ask every tool author to implement `verify_completion`.
We implement it for the high-value built-in mutating tools:

#### `write_file::verify_completion`

```rust
async fn verify_completion(&self, args: &Value) -> Result<CompletionStatus> {
    let path = args["path"].as_str().ok_or(...)?;
    let want = args["contents"].as_str().ok_or(...)?;
    match std::fs::read_to_string(path) {
        Ok(actual) if actual == want => Ok(CompletionStatus::Done {
            synthesised_result: format!("[verified on resume] wrote {path}"),
        }),
        Ok(_) => Ok(CompletionStatus::NotDone),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            Ok(CompletionStatus::NotDone)
        }
        Err(_) => Ok(CompletionStatus::Unknown),
    }
}
```

#### `apply_patch::verify_completion`

`apply_patch`'s args contain unique anchors. If the patch was
applied, the anchors no longer match (they're surrounded by the
patched content); if not applied, they still match. We can run
the same anchor search the live tool would run:

- Found, in original location → `NotDone` (patch wasn't applied)
- Not found → check if the *result* of applying the patch would
  match what's currently there → `Done` if so, `Unknown` otherwise
- Multiple matches (ambiguous) → `Unknown`, fall back to Ask

This is exactly the `apply_patch` engine's existing pre-check
logic, run in dry-mode. Refactor to a shared function used by both
the live path and verify.

#### `remember_fact` / scratchpad writes

Both keyed by stable id; if the id is already present with the
expected content, `Done`. Otherwise `NotDone`.

#### `run_shell::verify_completion` — explicitly do not implement

A general shell command has no fingerprint to verify. We could let
the user pass `--verify-cmd "..."` per call, but that's an agent-
level pattern (the LLM should learn to write idempotent commands or
ask), not a runtime feature. Keep `run_shell` at the trait default
(`Unknown`); the policy then falls through to `Ask` for it, which
is what we want.

### Resume control flow (extends g151/153)

```
recursive resume <id> [--replay=...]

  ├── load_messages, validate registry, acquire lock        (g151)
  ├── scan_orphan_tool_calls                                (g153)
  │
  └── for each orphan:
        action = policy.for_class(orphan.side_effect)
        match action:
          Redo:
            args drift check (args_hash vs current intent)
            if drift → fall through to Ask
            else → re-execute, append synthetic tool result
          Skip:
            append synthetic tool result `[skipped on resume]`
          Verify:
            tool.verify_completion(&args).await
            match status:
              Done { synthesised_result } → append it as result
              NotDone → behave as Redo
              Unknown → fall through to Ask
          Ask:
            interactive prompt (TTY) or abort exit 2 (non-TTY)

  └── proceed with normal run_resumed flow
```

### Args drift detection

`AuditMeta.args_hash` (from g153) was recorded at the original call.
On resume, the orphan's hash is compared with the canonical-JSON hash
of its current args (read straight from the assistant message's
`tool_calls`). They should always match for a true orphan (we read
the same transcript). The drift check exists for a different case —
a hand-edited transcript or a session migrated across versions where
the args field was rewritten. If hashes differ, we never auto-Redo;
we always fall through to Ask with a clear "args differ from the
recorded execution" message.

### Synthetic tool results

When the policy decides not to re-execute, we still must append a
`tool` message so the conversation is consistent for the next LLM
turn. The synthetic content is structured:

```json
{
  "role": "tool",
  "tool_call_id": "<original>",
  "content": "[resume:skipped] tool was scheduled but not executed on resume. side-effect class: External. Decide whether to repeat the request manually.",
  "audit": {
    "step_id": "<new ulid>",
    "started_at": <resume_time>,
    "finished_at": <resume_time>,
    "args_hash": "<copied from intent>",
    "side_effect": "External",
    "exit_status": { "type": "ok" }
  }
}
```

Three flavours:
- `[resume:skipped]` — Skip / Ask→skip
- `[resume:verified]` — Verify→Done, with the verifier's
  `synthesised_result`
- `[resume:redone]` — Redo / Verify→NotDone, plus the actual tool
  output

The LLM sees these prefixes in plain text and can react sensibly
("oh, it was skipped — should I retry?"). They are not magic —
just convention.

## Tests

Unit (in tools that gain `verify_completion`):

- `write_file_verify_done_when_file_matches`
- `write_file_verify_not_done_when_file_missing`
- `write_file_verify_not_done_when_file_differs`
- `apply_patch_verify_done_when_anchors_gone_and_result_present`
- `apply_patch_verify_not_done_when_anchors_present`
- `apply_patch_verify_unknown_when_ambiguous`
- `remember_fact_verify_done_when_present`

Policy unit:

- `policy_default_redoes_readonly`
- `policy_default_verifies_mutating`
- `policy_default_asks_external`
- `policy_cli_override_replay_skip_mutating`

Integration (`tests/replay_e2e.rs`):

- `replay_readonly_orphan_redoes_silently` — orphan grep, default
  policy, no prompt, transcript shows `[resume:redone]`.
- `replay_mutating_orphan_verifies_done` — `write_file` orphan
  whose target file already matches → `[resume:verified]`, no
  re-execution.
- `replay_mutating_orphan_verifies_not_done_then_redoes` — target
  file missing → re-execute, `[resume:redone]`.
- `replay_external_orphan_aborts_in_non_tty` — `run_shell` orphan,
  no `--replay-external` override, non-TTY → exit code 2.
- `replay_args_drift_falls_through_to_ask` — manually edit the
  transcript's args, `args_hash` mismatch → policy bypassed,
  treated as Ask.

## Acceptance

- `cargo build` green
- `cargo test` green; ≥ 13 new tests (7 verify + 4 policy + 5 e2e
  with overlap; minimum 13 net new)
- `cargo clippy --all-targets -- -D warnings` clean
- `cargo fmt --all -- --check` clean
- Manual smoke (cumulative with g151+152+153):
  ```
  recursive run "create files a.txt b.txt c.txt with their names"
  # kill mid-second-write
  recursive resume
  # resume should detect 1 orphan write_file, verify it (file b.txt
  # is missing), redo it, and proceed to file c.txt without prompting
  ```

## Out of scope (deferred)

- **Per-tool replay policy in config.toml**. The CLI flags are
  enough until we see real usage.
- **Async / long-pause primitives** (`wait`, durable sleep,
  external-event wait). That's 18.5c, an independent design.
- **Per-call human approval persistence** (durable approval events
  with artifact hashes, à la the LangGraph interrupt pattern). The
  current `plan_first` mode is the closest thing; making approvals
  durable across crashes is a separate goal.
- **Cross-machine resume / distributed agents**. Beyond local FS.
- **MCP-side `verify_completion`**. Would need a protocol extension
  (MCP has no "is this already done?" call). Track separately.

## Notes on safety claims

This goal makes the agent more **convenient** to resume, not more
**safe** in an absolute sense. The hard safety guarantee — "we will
never silently double-execute a destructive External call" — already
holds after g153 (default `Ask` for External). Adding `Verify` for
Mutating tools improves convenience without lowering that bar
because verifiers are honest about uncertainty (`Unknown` falls
through to `Ask`).

The places this goal could hurt:

1. A buggy `verify_completion` that returns `Done` when the
   side-effect didn't actually happen → silent data loss. Mitigation:
   verifiers are tiny pure functions over local state; tested
   carefully; `Unknown` is always the safe answer.
2. User passes `--replay-external=redo` and that re-runs a
   destructive shell call. Mitigation: this requires explicit
   opt-in; the flag's help text says "DANGEROUS"; non-TTY needs
   `--yes`.

We do not claim "Recursive is now durable" after this goal — true
durability would also need 18.5c (sleep/wait primitives) and richer
external-event handling. What we get is **the agent stops asking
the LLM to figure out crash recovery**, and that alone is a big win
for everyone using long tasks today.
