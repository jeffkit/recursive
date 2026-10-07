# Manual change — issue #110

- **Date:** 2026-10-07
- **Goal:** #110 fix(cli): `run`/`resume` 的错误路径跳过会话 finalize——55% 的
  `.meta.json` 永远停在 `active`，"尸体"与"在跑"不可分

## Files touched

- `crates/recursive-cli/src/main.rs`
  - `run_once`'s `run_result` `Err` arm now calls
    `cli::output::finalize_session_writer(session_writer, Crashed, None, Some(err))`
    before the cost finalize + `return Err(err)`. Before, only `cost.json` was
    written (#115) and the session envelope was left `active` forever. The
    `accept_user_messages` loop in this function already funnels its `?` into
    the same `run_result`, so this one arm closes every one-shot error path.
- `crates/recursive-cli/src/cli/resume.rs`
  - `run_resumed`'s primary `runtime.run(...)` `Err` arm drops the runtime
    (its event sink holds a writer clone) and finalizes as `Crashed` with the
    error text.
  - The `accept_user_messages` loop's bare `let turn = runtime.run(msg).await?;`
    is now a `match` — a mid-turn failure finalizes the envelope before
    propagating, instead of `?`-jumping past it.
- `crates/recursive-cli/src/cli/output.rs`
  - `finalize_session_writer` no longer demands unique ownership
    (`Arc::into_inner`); it locks the shared `Mutex<SessionWriter>`. A
    JSON-mode run hands a writer clone to the control session and its spawned
    stdin demux, which outlive the runtime — the old unique-ownership check
    refused and printed `session: writer still has other references; cannot
    finalize`, so those sessions stayed `active` even after this issue's
    other two fixes. Every caller drops the runtime (and thus the persistence
    sink) first, so locking cannot interleave with an append.

## Baseline repair (unrelated to #110, but required to build)

`main` at `dbe2ceb5` did not compile — a cross-PR landing ate two symbols:

- `src/http/session_mirror.rs` (added by #121) built a `SessionMeta` without
  the `finish_reason` / `error` fields that #111 had added.
- `src/http/mod.rs`'s `mirror_closing_session` read
  `session.prompt_tokens` / `session.completion_tokens`, which #114 had
  replaced with `session.usage: Arc<SessionUsage>`.

Repaired both (use `session.usage.snapshot()`; pass `finish_reason: None,
error: None`), with a unit test pinning the usage→`meta.cost` mapping. Without
this the mandatory gates cannot run at all.

## Tests added

- `crates/recursive-cli/tests/cli_session_surfaces.rs`
  - `run_finalizes_the_session_as_crashed_when_the_provider_fails` — a `run`
    against an unreachable provider must leave `status: "crashed"`, the error
    text on `.meta.json`, and a `cost.json`.
  - `run_finalizes_a_json_mode_session_as_crashed_when_the_provider_fails` —
    same contract under `--output-format stream-json`, which is the mode whose
    control session used to block finalization.
- `crates/recursive-cli/tests/cli_resume_surfaces.rs`
  - `resume_finalizes_the_session_as_crashed_when_the_provider_fails` — a
    `resume` whose provider returns a 400 must rewrite the appended-to
    session's `.meta.json` from `active` to `crashed` with the error text.
    (The stub grew a `fail` mode.)
- `src/http/mod.rs`
  - `mirror_closing_session_maps_session_usage_onto_meta_cost` — pins the
    baseline repair above.

## Notes

- Scope: the fix is the *class* of error path the issue names (run_once `Err`,
  resume `run` `Err`, resume mid-turn `Err`) plus the shared-writer
  finalization that both depend on. All three runtime-visible scenarios are
  covered by integration tests that spawn the real binary.
- A `resume` still does not write `cost.json`: `run_resumed` only builds a
  `CostTracker` when `session` is true, and `cmd_resume` passes `false`
  (it opens the writer itself and hands it in as `existing_writer`). That is a
  separate cost-accounting gap (same family as #114/#115), left untouched here
  — the resume test asserts status + error only.
- `~/.recursive` corpses: with this change a failed text *or* JSON run lands
  `status=crashed` + `error`, so `recursive agents` / `sessions list` can tell
  a dead session from a live one.
