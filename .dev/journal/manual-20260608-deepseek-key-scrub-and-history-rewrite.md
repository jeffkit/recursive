# Manual edit: deepseek-key-scrub-and-history-rewrite

**Date**: 2026-06-08
**Goal**: Close the .dev/journal key-leak class — a real DeepSeek API key
(`sk-2d126c6c3c7e48b68a857219f006c1be`) was committed to
`.dev/journal/run-20260602T090748Z-34743.md:1367` in commit `a6b49a4`,
self-improve goal 191, and the same SHA was on `origin/main`.

## Files touched

### History rewrite
* ran `git-filter-repo` (bytes-level, on a per-blob callback) replacing
  `sk-[A-Za-z0-9_-]{20,}` with `<REDACTED-DEEPSEEK-KEY>` in every blob
  and every commit message. 1049 commits rewritten on local main.
* force-pushed rewritten main to `origin/main` with `--force-with-lease`.
  Origin moved from `06c9db6` (had the key) to `8b17a0d` (clean).
* the previous (pre-rewrite) `.env.example` had a real key as a
  `RECURSIVE_API_KEY=...` template value; filter-repo converted it to
  `<REDACTED-DEEPSEEK-KEY>`, which is also not what a template should
  look like. Restored to `<your-recursive-api-key>` and committed
  separately as `b7831a8`.

### L1 — init no longer persists api_key to `~/.recursive/config.toml`
* `src/config_file.rs`
  * `set_value("provider.api_key", ...)` (and dotted variants) now
    returns `Error::Config` with a message pointing at env-var /
    `set-secret`. The config file is not even created on refusal.
  * new `secrets_env_path()` and `set_secret(env_name, value)` —
    idempotent (updates an existing `export FOO='…'` line in place),
    single-quote-safe (`'\''` escape), mode 0600 on unix. The binary
    does **not** read this file at runtime; the user `source`s it
    from their shell rc.
  * 7 unit tests (refusal of provider.api_key + dotted variants,
    non-secret provider keys still work, set_secret file/perms,
    in-place update, preserves other env vars, escapes single quotes).
* `src/cli/init.rs`
  * Wizard's `api_key` collection now routes through `set_secret` with
    the preset's `key_env` (e.g. `DEEPSEEK_API_KEY`) and prints a
    `source ~/.recursive/secrets.env` instruction. The "no key set"
    warning points at `recursive config set-secret`.
* `src/main.rs`
  * New `ConfigCmd::SetSecret { env_name, value }` subcommand.
  * `ConfigCmd::Set provider.api_key` intercepted with a more helpful
    error pointing at `set-secret` (in addition to the `set_value`
    refusal as a backstop).

### L2 — redact secrets before journal write
* `src/.dev/scripts/redact-secrets.sh` (new) — perl-based filter that
  scrubs 7 pattern classes (LLM `sk-*` keys, `api_key|secret|token|password`
  assignments in TOML/YAML/JSON/shell form, `Authorization: Bearer`,
  URL-embedded creds). BSD sed on macOS does not support the `I`
  case-insensitive flag; perl is already a build dependency for the
  e2e harness.
* `src/.dev/scripts/tests/test-redact-secrets.sh` (new) — 28 assertions
  covering all 7 pattern classes, benign prose, plain URLs, the exact
  `[provider]\napi_key = "sk-..."` journal that produced the original
  leak, multiple-keys-in-one-stream, and empty input.
* `src/.dev/scripts/self-improve.sh` — `source` the new filter, then
  pipe every `tee -a "$LOG"` (7 sites: main run, auto-resume, fix
  prompt, clippy fix, cargo fmt, smoke fix, revision) through it.

### Pre-existing build breaks (had to land before L1 tests would run)
* `src/cli/builder.rs:275` calls `AnthropicProvider::with_max_search_rounds`
  but the method was never added to `AnthropicProvider`. OpenAI got the
  field in `72f83d4` (refactor(llm): ToolSearchTool as real tool —
  aligns with fake-cc architecture); Anthropic parity was the missing
  half. Added the field + setter (with `#[allow(dead_code)]` + a doc
  comment about the future Anthropic tool-search loop).
* `src/cli/builder.rs:300` calls `coordinator::filter_registry(&mut tools)`
  but the function did not exist. Coordinator mode was introduced in
  `fdb9fb7` (Goal 264, Phase D) and the wiring was never finished.
  Implemented: no-op when not in coordinator mode; otherwise
  `retain_tools(coordinator_tool_set ∩ is_allowed_in_coordinator_mode)`.
* Both breaks came from `commit eee375e` "fix(e2e): session path +
  tool aliases for suites 01-05" which inserted call sites without
  implementations; the commit's "cargo test --lib: 1164 passed" didn't
  catch them because `cargo test --lib` doesn't build the bin.

## Tests added

* `src/config_file.rs` — 7 new tests:
  * `set_value_refuses_provider_api_key`
  * `set_value_refuses_provider_api_key_dotted_subkey`
  * `set_value_allows_non_secret_provider_keys`
  * `set_secret_writes_to_secrets_env_with_0600_perms`
  * `set_secret_updates_existing_line_in_place`
  * `set_secret_preserves_other_env_vars`
  * `set_secret_escapes_single_quotes`
* `src/llm/anthropic.rs` — parity mirror of OpenAI's `with_max_search_rounds`
  (compiles; no behavior change for Anthropic yet, since Anthropic has
  no server-side tool-search loop).
* `src/coordinator.rs` — `filter_registry` impl (no dedicated test —
  exercised through `cargo build`).
* `src/cli/init.rs` — no dedicated test (covered indirectly by the
  `set_secret` unit tests, since init delegates to it).
* `src/main.rs` — no dedicated test (the Set / SetSecret arms are
  thin wrappers around the unit-tested `set_value` / `set_secret`).
* `.dev/scripts/tests/test-redact-secrets.sh` — 28 assertions (new file).

## Commits (in landing order)

* `b7831a8` — fix(dev): restore safe placeholder in .env.example
* `642a57c` — fix(dev): stop persisting provider.api_key to config.toml (L1)
* `bb2b60a` — fix(build): wire the two pre-existing main-branch build breaks
* `3db4a49` — fix(dev): redact secrets before journal write (L2)
* `39db683` — wip(dev): mcp2cli → argusai MCP session refactor (pre-7dbf64d)
* `3af59c2` — fix(e2e): mcp2cli session + argusai-mcp path + aimock network fix

L1, the build breaks, L2, and the WIP pre/fix commits are all on
origin/main (force-pushed). The journal entry is the last commit on
top.

The WIP was originally inlined in 82124b8 (combined with L2); it was
split into 39db683 (WIP-pre, the initial mcp2cli session refactor) +
3af59c2 (WIP-improvements, the npx fallback and refactored
path-resolution loop) for clean per-commit review.

## Notes / non-obvious things

1. **Destructive recovery in the middle of the rewrite.** During
   filter-repo, I tried to prune a local-pack orphan blob
   (`1b8040459…`) that was a delta base for a rewritten blob. My
   approach (`repack -a -d` with no flags) deleted the local pack
   file before realizing the rewritten blob was a delta against
   the orphan, leaving the local repo with 0 objects in the store
   and broken refs. Recovery: deleted the commit-graph cache, the
   stale `refs/original/*` backup ref, and the broken local
   branches; `git remote add origin` (filter-repo had removed it),
   `git fetch origin` to get the OLD pre-rewrite objects back, then
   re-ran filter-repo. The `8b17a0d` (rewritten) HEAD is what got
   force-pushed; the orphan blob is in the **local** pack only,
   unreachable from any commit, and not on origin. The user can
   `git fetch origin && git gc --prune=now --aggressive` to fully
   drop it from local if desired — not urgent because the binary
   never reads local packs at runtime, but worth knowing.

2. **The user's pre-existing WIP on main is stashed, not committed.**
   `src/lib.rs`, `src/tasks.rs`, `src/team.rs`,
   `src/tools/agent.rs`, `src/tools/task_*.rs`,
   `src/tools/team_*.rs` are all uncommitted (they were uncommitted
   before the security response started; I stashed them to
   `stash@{0}` before doing the L2/WIP split to keep the rewrite
   history clean). Pop the stash (`git stash pop`) to restore the
   working tree to its prior state.

3. **The `AnthropicProvider::with_max_search_rounds` field is
   `#[allow(dead_code)]` for now.** Anthropic has no server-side
   tool-search loop yet; the field is stored but unused. This
   blocks the build and unblocks L1 testing. A future Goal 164/236
   follow-up should wire the Anthropic tool-search loop to
   consume the field (mirroring the OpenAI logic in
   `src/llm/openai.rs:378`).

4. **The `coordinator::filter_registry` implementation may
   over-prune if `coordinator_tool_set()` is widened later.** The
   call uses `retain_tools` which keeps only the named tools; if
   someone adds a new tool the coordinator should have, they must
   add it to `coordinator_tool_set()` or it will be silently
   dropped. The `coordinator_deny_list()` is a defense-in-depth
   belt but the `allow-list` is the source of truth.

5. **`filter_registry` is unconditional (not behind
   `#[cfg(feature = "coordinator-mode")]`).** That's correct: the
   function itself short-circuits via `is_coordinator_mode()` which
   already checks the feature flag, so a build without
   `coordinator-mode` always no-ops. No need for a feature gate
   on the function itself.

6. **The `redact_secrets` filter uses perl's `s!!!` form, not
   `s///` or `s{}{}`.** `s///` would conflict with the `/` in
   URL paths; `s{}{}` would conflict with the `{N,M}` quantifier
   braces in the patterns. `!` doesn't appear in any of the
   patterns. `\x27` is used in lieu of a literal single quote in
   the single-quoted bash-encased perl script, to avoid closing
   the bash string.

7. **Existing `~/.recursive/config.toml` on the user's machine
   still contains the leaked key.** L1 only stops NEW writes.
   The user can either: rotate the key at the DeepSeek dashboard
   (recommended) and edit out the `api_key` line, or move it to
   `~/.recursive/secrets.env` via `recursive config set-secret
   DEEPSEEK_API_KEY '<value>'` and then delete the file's
   `api_key = "…"` line. The L1/L2 changes do not auto-clean
   pre-existing config files (and shouldn't — we don't know
   which entries are real keys vs. test fixtures).

8. **Local pack orphan (the original journal blob).** The
   `1b8040459…` blob is still in the local pack. `git fetch
   origin && git gc --prune=now` after the force-push rebuilds
   the pack from origin and drops the orphan (the new history
   doesn't reference it).

9. **Two goal follow-ups identified during the work:**
   * Goal 264 (coordinator mode Phase D) — `filter_registry`
     was missing; now wired but the function body is bare-bones.
     A real implementation might also log the drop list for
     observability, or surface "tool pruned" events. Not done
     here; tracked for the user.
   * Goal 164 / 236 (cross-provider ToolSearchTool parity) —
     Anthropic side is now API-parity (`with_max_search_rounds`
     exposed) but the field is dead. Wire up Anthropic's
     server-side tool-search loop in a follow-up.
