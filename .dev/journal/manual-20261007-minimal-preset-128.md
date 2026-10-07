# Manual journal — issue #128 minimal preset (one-line prompt + core tools + no injection)

- **Date**: 2026-10-07
- **Goal**: `#128 feat(runtime)` — a `minimal` agent preset. Every session
  unconditionally injected the assembled base prompt + 6 memory layers
  (`src/config.rs` `from_env`), the skill catalog (`skill_reminder`, per turn),
  the project context (`AGENTS.md` / `CLAUDE.md`, 16 KB cap), the
  `<environment>` segment, and installed the compactor — so a chat turn or a
  single-file edit paid the whole fixed cost. Depends on #127 (the preset
  abstraction), which had landed (`src/preset.rs`, `9baff4d2`).
- **Baseline**: `285ca334` (repo HEAD when the run started; #127 is an
  ancestor).

## What #127 already gave (and what it did not)

`preset::apply` was already the single assembly point for *context management*
(compactor / microcompactor / transcript cap / file + skill re-injection) and
the plan-mode tool gate, and `POST /sessions`'s `preset` field selected the
preset. What was missing for a minimal session:

1. `PromptProfile.persona_suffix` only *appended* to the assembled prompt —
   there was no way to replace it (the memory-free base prompt is not stored
   on `Config`, only `default_system_prompt()`), and no way to skip the
   project-context / skills / environment segments.
2. `ToolProfile` only had `plan_mode_tools` — no tool subset.
3. The skill catalog ships as a per-turn `<system-reminder>`, gated solely by
   a non-empty `globs_skills` (i.e. by `.skills(...)` on the builder).
4. Nothing reported a preset's system-prompt weight.

## What landed

### `src/preset.rs` — the declaration

- `PromptProfile` gains `complete: Option<&'static str>` (the
  DeepSeek-Harness `complete: true` form: this text **is** the whole system
  prompt — assembled base, project context, memory layers, sub-agent note,
  skill catalog and `<environment>` segment are all discarded, whatever the
  caller put in the request body) and `skill_catalog: bool`.
- `ToolProfile` gains `allow: Option<&'static [&'static str]>` — the LLM-facing
  tool names the session's registry is pruned to.
- `MINIMAL_PROMPT` = `"You are a helpful software engineer assistant."`;
  `MINIMAL_TOOLS` = `["Read", "Write", "Edit", "Bash"]` (the registry keys *are*
  the spec names — `ReadFile`/`RunShell` are the Rust types, not the wire
  names).
- `static MINIMAL: AgentPreset` — `complete` prompt, `skill_catalog: false`,
  `auto_skill_injection: false`, no plan tools, the four-tool allow-list, and a
  `ContextProfile` whose every field is `None`. Registered in `BUILTIN`
  (one declaration + one line, per #127 acceptance 3).
- `MINIMAL_CAPABILITIES` — the inventory, mostly `Disabled` rows, so "what a
  minimal session does *not* have" is discoverable instead of inferred.
- `apply_prompt(AssembledPrompt, &ResolvedPreset) -> AssembledPrompt` (was
  `String -> String`): a `complete` profile replaces the prompt **and its
  segment breakdown**, so the Goal-328 `ContextBreakdown` estimator sizes what
  the request actually carries (the TUI's context panel would otherwise show
  the discarded assembly).
- `preview_system_prompt` / `system_prompt_tokens` — what a session created
  under a preset would send on its first request, and its token weight.

### `src/runtime/builder.rs` — the tool subset, applied where it cannot be undone

`with_tool_allow(Vec<String>)` + `tool_allow` field; `build()` calls
`retain_tools` right after the kernel exists and **before** the sinked
re-registrations (TodoWrite, Present, ChangeLedger, plan-mode). Those are all
guarded by `surface_filtered`, so a preset's four-tool surface stays four
tools — a channel cannot resurrect anything by re-registering later.

### Channels

- `src/http/handlers.rs::build_session_runtime_parts` is now the **single**
  HTTP-family prompt finalization point: it applies the prompt profile and
  installs the skill catalog *before* `preset::apply` (so `skill_catalog:
  false` really clears it). `build_session_runtime` no longer pre-applies the
  profile and no longer sets `.skills(...)` after the assembly.
- Consequence: `/agui` (`src/http/agui.rs`) and trigger runs now honour the
  prompt profile too — they previously called `build_session_runtime_parts`
  without `apply_prompt`, so a `persona_suffix` was silently lost there.
- `crates/recursive-cli/src/cli/builder.rs`,
  `crates/recursive-tui/src/runtime_builder.rs` (both build paths) — adapted to
  the new `apply_prompt` signature.

### Observability

`PresetInfo.system_prompt_tokens` (`src/http/mod.rs`) + the OpenAPI schema:
`GET /presets` now reports each preset's per-request system cost, so #113/#114's
cost surface can attribute the fixed part instead of guessing it. The
`<environment>` segment is transport-specific and therefore not part of the
preview.

## Measured evidence (acceptance 1)

`preset::tests::minimal_system_prompt_is_an_order_of_magnitude_smaller` builds
a realistic workspace (a 4.8 KB `AGENTS.md`, memory / scratchpad / facts stores
seeded, a skill) and runs `Config::from_env()` for real:

```
system prompt tokens — standard: 2347, minimal: 12
(ratio 195.6x, standard base 9387 bytes, minimal 46 bytes)
```

The same ratio is asserted on the live `GET /presets` payload in
`http::handlers::tests::list_presets_exposes_the_capability_inventory`
(standard's workspace is the repo, so it pays for the root `AGENTS.md`).
Threshold in both: `standard >= minimal * 10`. The printed counts above are one
machine's reading — they move with the workspace and the ambient `RECURSIVE_*`
env (a later re-measure printed `2412 / 12`), which is why the assertion is the
`≥10×` threshold and no exact ratio is quoted in `CHANGELOG.md`.

## Tests added

- `src/preset.rs`:
  - `a_complete_prompt_profile_replaces_the_whole_assembly` — project context,
    skills, sub-agent note and a pushed `<environment>` segment all vanish; the
    segment breakdown is replaced with the one-liner.
  - `minimal_prunes_the_built_runtime_to_the_core_tools` — the *built* runtime
    exposes exactly `Bash/Edit/Read/Write` from a full standard registry, and
    the kernel carries no skill catalog.
  - `minimal_system_prompt_is_an_order_of_magnitude_smaller` — the measurement
    above (prints the numbers so a `--nocapture` run leaves evidence).
  - `the_simple_task_set_completes_under_both_presets` — the three simple task
    shapes (read-modify-run, search, multi-step) driven through a real runtime
    under `standard` and `minimal`: same scripted script, same
    `NoMoreToolCalls`, same edited file.
- `src/http/handlers.rs`:
  - `minimal_session_carries_a_one_line_prompt_and_the_core_tools` —
    acceptance 3 on the real HTTP assembly path: the first system message is
    the one-liner (no `# Project context`, no `Memory summary`, no
    `Available skills`, no `<environment>`), the builder ships no skill
    catalog, and the tool specs are the core four — even though the channel
    handed over the full registry and the fully assembled prompt.
  - extended `list_presets_exposes_the_capability_inventory` with the minimal
    row + the token ratio.
- `crates/recursive-cli/src/cli/builder.rs`:
  - `minimal_preset_replaces_the_cli_prompt_and_surface` — the real CLI
    `build_runtime` path under `RECURSIVE_AGENT_PRESET=minimal`: the built
    runtime's system message is the one-liner (no `# Project context`), and its
    tool specs are exactly `Bash/Edit/Read/Write`.
- `crates/recursive-tui/src/runtime_builder.rs`:
  - `minimal_preset_replaces_the_tui_prompt_and_surface` — the same assertion
    on the real TUI `build_runtime` path, so both crate-local channels are
    pinned against a profile that only reaches part of the assembly.
- Updated for the new shapes:
  `a_new_preset_is_one_declaration_with_no_builder_branch`,
  `prompt_suffix_is_a_no_op_for_the_standard_preset`,
  `an_unknown_preset_id_is_an_error_listing_the_known_ids` (the known list is
  now `standard, minimal`).

## Gates

- `cargo test --workspace` — **4563 passed, 0 failed** (the two crate-local
  tests below on top of the original 4561).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` —
  clean.
- `cargo fmt --all` — applied (no unrelated files touched).
- `.dev/scripts/cli-test-presence.sh` / `.dev/scripts/tui-test-presence.sh` —
  both pass: the CLI/TUI edits ship crate-local tests (above), so neither
  crate needs the `RECURSIVE_*_TEST_PRESENCE=0` opt-out.

## Honest gaps

1. **Acceptance 2 (success rate on a simple task set) is only covered by its
   wiring half.** `the_simple_task_set_completes_under_both_presets` uses
   `MockProvider`, which never sees the system prompt, so it proves the pruned,
   memory-less session is still a working agent — not that a real model scores
   the same. The quality half needs the benchmark issue's task set and a live
   model; neither exists in this repo yet (no `bench/` target, and `e2e/` is
   pass/fail Docker + argusai, unavailable here). The evidence a reviewer can
   act on today is the token measurement plus the wiring test.
2. **`recursive loop` (`crates/recursive-cli/src/main.rs:2415`) and the ACP
   server (`src/acp/server.rs`) still ignore agent presets entirely** — a
   pre-existing #127 gap, not introduced here. `RECURSIVE_AGENT_PRESET=minimal`
   therefore has no effect on those two surfaces; fixing them means adding the
   whole `preset::apply` + `apply_prompt` pair, not just the prompt half, and
   that is a separate change.
3. Under `minimal`, `complete` owns the prompt, so everything the caller sends
   for it is silently dropped: an explicit request-level `systemPrompt` /
   `appendSystemPrompt` (REST `/run` and `/sessions`, and the AG-UI
   client-declared prompt), plus any AG-UI client-declared tools (the request's
   tool list is narrowed to the preset's four-tool surface, with no error and
   no warning). This is the intended `complete` semantics — a benchmark that
   injected a per-task prompt override would corrupt its own baseline — but a
   client that assumed its own override survives must not pick this preset. The
   AG-UI analogue of the prompt half is pinned by
   `agui_request_prompt_cannot_drop_server_owned_segments`.

## Invariant audit

| invariant | status |
|---|---|
| 1. Agent loop stays small | ✅ — no `run_inner` change |
| 2. Orthogonality | ✅ — preset / builder only |
| 3. Sandbox | ✅ — untouched |
| 4. Tests required | ✅ — every new public fn has a same-file test |
| 5. No `unwrap()`/`expect()` in product code | ✅ |
| 6. No new deps | ✅ |
| 7. Finish reasons are data | ✅ |
| 8. Tool-call ↔ tool-result pairing | ✅ — the subset prune drops whole tools, never a transcript half |
