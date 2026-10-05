# Manual journal — 2026-10-05 — replay --resume-from orphan scan (#126)

- Date: 2026-10-05
- Goal: #126 — #91 (`4e1aee98`) fixed the orphan chain for
  `resume --from-file`, but `replay --resume-from N <goal>` was left
  untouched: the seed slice went straight to `run_resumed`
  (`crates/recursive-cli/src/main.rs`), with no orphan scan. A slice that
  stops on "tool_call issued, result never landed" (the SIGKILL/power-loss
  shape `--resume-from` exists for) is an unpaired transcript → the provider
  answers HTTP 400 "tool_use ids were found without tool_result blocks".
  Unlike `resume`, `replay` had no `--orphans` flag and no default fallback,
  so an unattended replay of a crashed transcript still died on the first
  provider call.
- Files touched:
  - `src/session/orphan.rs` — new `scan_orphan_tool_calls_in_messages` and
    `splice_orphan_results` (the `Vec<Message>` counterparts of
    `SessionReader::scan_orphan_tool_calls` /
    `load_messages_with_orphan_results`), plus a private
    `orphan_result_insert_at_messages` mirroring `reader`'s insert-index rule.
  - `src/session/mod.rs` — re-export the two new helpers.
  - `crates/recursive-cli/src/cli/resume.rs` — `replay_orphan_policy`
    (explicit `--orphans` wins, default `Skip`) and `prepare_replay_seed`
    (scan + policy + splice). Reuses the existing `redo_orphan` /
    `prompt_orphan_choice` / `OrphanPolicy` machinery, so `skip` / `redo` /
    `ask` / `abort` behave exactly as `resume`'s.
  - `crates/recursive-cli/src/main.rs` — `Cmd::Replay` gains `--orphans`
    (values `skip` default, `redo`, `ask`, `abort`); the `--resume-from N`
    arm now builds the tool registry, resolves the policy, and runs
    `prepare_replay_seed` before `run_resumed`.
  - `crates/recursive-cli/tests/cli_session_surfaces.rs` — CLI wiring tests.
- Change:
  - `replay --resume-from N` now scans the sliced seed for orphans (last
    assistant message with `tool_calls`, no later `tool` answering an id)
    before the provider sees it. Default = `skip`: each orphan is answered
    with `[interrupted: no result recorded]`, i.e. the unattended semantics
    the gap asked for (not `resume`'s ask-on-TTY / abort-otherwise heuristic).
  - `--orphans=redo` re-executes the recorded call against the current
    registry and splices its real output — the same promise `resume` makes
    (External calls keep their human confirmation; no TTY ⇒ the explicit
    opt-in is honoured with a warning). `--orphans=abort` refuses the run and
    `--orphans=ask` prompts, both as in `resume`.
  - The answered seed *is* the run's transcript (`build_runtime`'s seed), so
    the synthetic result is persisted with it — `--transcript-out` (and the
    `--session-out` crash save) carry the paired transcript. Unlike `resume`
    there is no pre-existing session directory to append to, and appending a
    lone tool result to the *fresh* session `replay` creates would itself be
    an unpaired row, so no session write-back is done here.
- Tests added:
  - `src/session/orphan.rs` (11): detects an unanswered call / a batch's one
    unpaired call; empty when all answered, when no assistant `tool_calls`,
    and when the anchor is a later call-less assistant; a seed whose first
    message *is* the assistant (pins the `asst_idx + 1` slice start); splice
    into the no-result, partial-batch, no-answers, and no-anchor shapes; and
    a dedicated insert-index test pinning the arithmetic (after the
    assistant / after its last result / after the whole partial batch, never
    anchored on a call-less trailing assistant).
  - `crates/recursive-cli/src/cli/resume.rs` (6): `replay_orphan_policy`
    default + validation; `prepare_replay_seed` skip / redo / abort and the
    paired-seed no-op; and the acceptance case
    `replay_resume_from_skip_round_trips_the_provider_and_persists_the_result`
    (loopback provider, asserts the request carries
    `"role":"tool"` / `"tool_call_id":"tc-1"` / the synthetic note, and that
    `--transcript-out` persists it).
  - `crates/recursive-cli/tests/cli_session_surfaces.rs` (2): an orphan seed
    tail is answered by default (stderr shows the scan, `--orphans=skip` and
    `resuming from 3 seeded message(s)`), and `--orphans=abort` refuses
    before the run starts.
- Notes:
  - #91's journal flagged this as the follow-up and noted the
    `cli-mutants`-timeout risk of touching `main.rs` (AGENTS.md failure mode
    #8). That gate is already budgeted at 150 min in `.flowcast/gates.json`,
    so this lands the fix on the intended entry point rather than hiding it.
  - The scan/splice rules are duplicated for the in-memory representation on
    purpose: the on-disk pair reads `TranscriptEntry`s (and carries the
    persistence `id` in `OrphanToolCall::assistant_msg_id`, which in-memory
    messages have no equivalent for), and #91's tested code is left
    untouched.
  - The tail is taken as `messages[asst_idx..].iter().skip(1)`, not
    `messages[asst_idx + 1..]`, so the insert-index arithmetic has no
    `usize`-index `+ 1` that `cargo mutants` would flag
    (`asst_idx * 1` is equivalent and unkillable); the pinning test asserts
    the exact indices.
  - No new dependency; `blake3` (already used by the on-disk scanner) hashes
    the call arguments for `args_hash`.
