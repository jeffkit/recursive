# Manual change — issue #49: sandbox doc env-var name + per-tier env schema text

- **Date**: 2026-10-01
- **Goal**: Make the sandbox-tier documentation match the code on two
  points that cost debugging time: the container network opt-in var, and
  the `Bash` `env` argument's claim about "the inherited env".

## Findings first (issue premise vs current HEAD)

- **① was already fixed on `main`** by commit `7b83deb` (#51): both
  `docs/architecture/execution-environments.md` occurrences of
  `RECURSIVE_CONTAINER_NETWORK` (lines 40 / 101 at the time) were
  rewritten to `RECURSIVE_SANDBOX_NETWORK`. `grep -r
  RECURSIVE_CONTAINER_NETWORK` over the worktree today hits only the
  journal note that *describes* the rename
  (`.dev/journal/manual-20260930-issue51-threat-model.md:12`) — no
  live doc/code usage. Nothing to change for ①; verified, not assumed.
- **② still held**: `src/tools/shell.rs:92` described the `env` arg as
  "add to (or override) the inherited env", which is false for the
  container/microvm tiers (`env_pairs` starts empty; the sandbox env is
  exactly the explicit pairs). Fixed here.

## Files touched

- `src/tools/shell.rs` — env arg description now states the per-tier
  contract: local tiers (unset / none / policy) add to the inherited
  host env; sandboxed tiers (container / microvm) have **no** inherited
  env — only explicitly passed vars exist and host env (credentials
  included) is never forwarded. Aligned with `.dev/AGENTS.md` invariant
  #3 extension (issue #51) and the env-var-invariant section of the
  sandbox doc.
- `src/tools/shell.rs` (tests) — new
  `env_schema_description_matches_per_tier_reality`: pins that the
  schema text does NOT blanket-claim "the inherited env", DOES state
  local-tier inheritance, and DOES state sandbox non-inheritance.
  Also: `timeout_kills_child_process` now polls for the PID marker
  file (≤5 s) instead of reading it once — under a loaded host the
  150 ms timeout can fire before the child's `echo $$` lands, which
  made the test fail ~1-in-10 full-`--lib` runs (reproduced on clean
  HEAD before touching anything; 8 consecutive clean full runs after).
- `docs/architecture/execution-environments.md` — added an
  `inherited env` row to the capability matrix (yes for none/policy,
  **no** for container/microvm, with the mechanism) so the difference is
  visible in the matrix; capability/isolation network rows now carry the
  explicit `RECURSIVE_SANDBOX_NETWORK=on` value; env-var invariant
  section now anchors the schema wording to the new test.
- `src/tools/http_call.rs` — one-line import fix surfaced by the
  invariants gate: `use crate::acp::ToolKind` → `use
  crate::tools::tool_kind::ToolKind` (same re-exported type;
  `acp::mod` re-exports from `tools::tool_kind` since `8d56ba2`). The
  #63 HttpCall branch predates the #53 ToolKind de-ACPE-ing and landed
  after it, so the gate caught the leftover. Restore-point: file had
  exactly one occurrence; no behavioural change.

## Tests added

- `env_schema_description_matches_per_tier_reality` (unit, same file
  per convention). Existing anchors re-verified:
  `tests/issue51_sandbox_env_inheritance.rs` (behaviour) +
  `tests/invariants/loop_size_orthogonality.rs::
  tools_do_not_import_transport_adapters` (the gate that caught ④).

## Notes

- The `timeout_kills_child_process` flake was diagnosed as pre-existing
  (fails on clean HEAD too — see above) and fixed in the same change;
  the test now distinguishes "child not yet written the marker" (wait)
  from "child was killed before/without writing" (the ≤5 s poll bound
  still fails loudly).
- The ① acceptance criterion ("`rg RECURSIVE_CONTAINER_NETWORK` zero
  hits repo-wide") cannot literally include the historical journal that
  documents the rename; live surfaces (docs/, src/, e2e/, .flowcast/)
  are clean.
