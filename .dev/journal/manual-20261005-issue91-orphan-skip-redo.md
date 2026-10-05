# Manual journal — 2026-10-05 — resume orphan skip/redo (#91)

- Date: 2026-10-05
- Goal: #91 — `recursive resume --orphans=skip/redo` were pure prints. A
  session killed during tool execution ends on an assistant `tool_call` with
  no `tool` result; `skip` printed "treating as completed" but seeded the
  unpaired transcript anyway (provider HTTP 400, `src/llm/anthropic.rs:891`),
  and `redo` printed "will re-execute on resume" with no execution path. The
  non-TTY default (`abort`) refused the run outright for unattended callers.
- Files touched:
  - `src/session/orphan.rs` — `ORPHAN_SKIPPED_RESULT`,
    `ORPHAN_REDO_FAILED_PREFIX`, `OrphanToolCall.call` (the verbatim
    `ToolCall`, so a redo can replay the arguments).
  - `src/session/reader.rs` — `SessionReader::load_messages_with_orphan_results`
    + private `orphan_result_insert_at`.
  - `src/session/mod.rs` — re-export the two constants.
  - `crates/recursive-cli/src/cli/resume.rs` — orphan policy now produces
    `(tool_call_id, result)` resolutions; new `redo_orphan` /
    `replay_orphan_call`; answers are seeded *and* appended to the session
    JSONL.
  - `.dev/scripts/self-improve.sh` — every `resume --from-file` call passes
    `--orphans=skip` explicitly (non-TTY default is `abort`).
- Change:
  - `skip` answers each orphan with a synthetic `tool` message
    (`[interrupted: no result recorded]`), so the seed is a paired transcript
    (invariant #8) instead of one the provider rejects with HTTP 400.
  - `redo` re-executes the recorded `ToolCall` through the current registry
    (`ToolRegistry::invoke_with_audit`, i.e. the real permission pipeline)
    and records its output. A failing replay is not fatal — the error text
    becomes the tool result, `[interrupted: redo failed] <err>`. `External`
    calls keep their human confirmation (TTY prompt, reusing the
    redo/skip/abort chooser); with no TTY the explicit `--orphans=redo` opt-in
    is honoured and the warning is logged.
  - Answers are spliced in right after the last `tool` result of the issuing
    assistant message (or right after the assistant when the crash left no
    result), and appended to `transcript.jsonl` via `open_existing`'s writer —
    otherwise the seeded repair is lost and the next resume re-detects (and
    under redo, re-executes) the same orphans.
  - Both patches feed `run_resumed` through one path
    (`load_messages_with_orphan_results`), so the in-memory seed is coherent
    whether or not `--no-session` skipped persistence.
- Tests added:
  - `src/session/reader.rs` (5): empty answers == plain load; answers every
    unpaired call; follows a partial result batch (result lands *after* the
    recorded one); appends without an assistant call; index arithmetic pinned.
  - `crates/recursive-cli/src/cli/resume.rs` (5):
    `cmd_resume_skip_answers_the_orphan_and_round_trips_the_provider` — the
    acceptance case: crashed transcript + loopback provider, asserts the
    request body carries `"role":"tool"` / `"tool_call_id":"tc-1"` /
    `[interrupted: no result recorded]` and that the resume completes;
    `cmd_resume_redo_replays_the_orphaned_call` — recorded result contains the
    re-read file content; `redo_orphan_replays_a_readonly_call`;
    `redo_orphan_replays_external_calls_without_a_tty`;
    `redo_orphan_answers_with_the_error_when_the_replay_fails`.
- Notes:
  - `replay --resume-from` (`main.rs`) still has no orphan scan. Fixing that
    means editing `main.rs`, which trips the known `cli-mutants` timeout
    (AGENTS.md failure mode #8: any `main.rs` change puts ~150 extra mutable
    points in scope). Left as a follow-up; `run_resumed` would be the natural
    choke point for an implicit repair.
  - `.dev/scripts/self-improve.sh`'s auto-resume still only fires on
    `BudgetExceeded`. Extending the trigger to a crashed run was deliberately
    not done: the most common crash cause is auth/quota, and auto-resuming it
    would burn a full extra attempt on the flow's most frequent failure mode.
    Passing the policy explicitly is the part the gap asked for.
  - Two resume tests pin `RECURSIVE_HOME` (the checkpoint wiring resolves the
    shadow-git dir through user-data) and serialise on a module-local tokio
    mutex — this crate cannot use `recursive::test_util`'s env lock, which
    needs the `test-utils` feature.
