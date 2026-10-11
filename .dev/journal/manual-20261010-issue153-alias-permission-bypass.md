# Issue #153 — tool aliases must not bypass deny rules, protected-path checks, or L1 write policy

- **Date**: 2026-10-10
- **Goal**: Issue #153 (P0 security). Permission checks ran *before* alias
  resolution: `ToolRegistry::invoke_with_audit` passed the raw name to
  `PermissionPipeline::check`, and only the execution half
  (`find_by_name`) resolved aliases. Because the registry registers
  `read_file` → `Read` and `write_file` → `Write` (`register_with_aliases`),
  a single aliased call skipped all three controls at once:
  - `safety_content_for_tool` matched literal `"Write"`/`"Read"`/`"Edit"`,
    so `safety_content` was `None` and Phase 1's protected-path check
    (`.git`, `.ssh`, …) returned early — even though that check is
    documented to hold *without* a `permissions` config;
  - `matches_pattern` compared literally, so `deny = ["Write"]` never
    matched `"write_file"`;
  - the L1 `fs.write_allow` check classified the write via
    `matches!(tool_name, "Write" | …)`, so an aliased write looked like a
    read (or a non-write) and `write_allow` was never applied.

  Impact: `write_file {"path": ".git/hooks/pre-commit", …}` landed the
  hook; `read_file` could read `.git`/`.ssh`. Fix: **decide policy on the
  canonical (primary) tool name — aliases only ever affect lookup.**

## Files touched

- `src/tools/registry.rs` — new `pub fn canonical_name(&self, name) -> Option<&str>`
  (alias → primary; `None` when the name is already primary, so callers
  write `canonical_name(n).unwrap_or(n)`). `is_readonly` /
  `is_readonly_for_call` now resolve the alias first (they used
  `self.tools.get(name)` directly, so `is_readonly("read_file")` was
  `false`).
- `src/tools/policy_domain/permission_pipeline.rs` — `check()` normalizes
  to the canonical name on its first line and uses it for **every** phase:
  `safety_content_for_tool`, `check_protected_path`, the auto-classifier,
  `any_interactive`, `check_static`, the permission hook, the headless
  `HookInput`, and Phase 7 `recheck_policy`. `recheck_policy` also
  canonicalizes internally (it is public) so `write_file` is classified as
  a write. The raw name is kept only for user-facing error text.
- `src/permissions/mod.rs` — `check_static` delegates to a new
  `check_static_inner(tool_name, alias, …)`; new `check_static_aliased`
  passes the raw alias so **rules keyed on the alias keep firing** — a
  config `deny = ["write_file"]` (see `e2e/tests/35-permissions.yaml`)
  would otherwise silently lose its deny. Both names are tested in one
  pass (`name_matches`), so deny-union still beats allow-union.
  `matches_pattern` is unchanged.
- `src/tools/policy_domain/audit.rs` + `src/tools/dispatch.rs` —
  `AuditMeta` gains backward-compatible `tool_name` / `canonical_name`
  (`#[serde(default, skip_serializing_if = "Option::is_none")]`);
  `dispatch_after_permission_check` and the gated-denial record populate
  them. Observability only — no policy decision reads them.
- `src/tools/execution/fs.rs` — `WriteFile::execute` re-checks the
  resolved path against `check_protected_path` (defense in depth: the
  Phase 1 safety surface is required to hold on its own, and a direct
  `Tool::execute` call never reaches the pipeline).

## Tests added

- `permission_pipeline::tests::alias_cannot_bypass_deny_rule` — `deny =
  ["Write"]`; both `Write` and `write_file` → `Deny`.
- `permission_pipeline::tests::alias_cannot_bypass_protected_path` —
  `.git/hooks/pre-commit` and `.ssh/authorized_keys`, both names, no
  `permissions` config installed; `DecisionReason::SafetyCheck`.
- `permission_pipeline::tests::alias_is_treated_as_write_by_l1_policy` —
  `write_file` outside `fs.write_allow` → `Err` (would pass if classified
  as a read).
- `permission_pipeline::tests::alias_keyed_deny_rule_still_fires` — a
  `deny = ["write_file"]` config keeps denying its alias.
- `registry::tests::alias_readonly_classification_matches_primary` and
  `canonical_name_resolves_alias_and_ignores_primary`.
- `dispatch::tests::audit_records_both_names` — raw `write_file`,
  canonical `Write`.
- Regression: `protected_path_denied_in_default_mode` /
  `_in_bypass_mode` and the rest of `cargo test --lib` (3255 tests) green.

## Notes

- `e2e/tests/35-permissions.yaml` is untouched: its scenario B
  (`deny = ["write_file"]`) still denies because rule matching honors both
  the canonical and the raw alias.
- Not changed: rule *syntax* (still exact / `prefix*`) and the protected
  path list.
- Follow-up candidate (out of scope, same "literal name" family):
  `dispatch::record_touched` matches `"Write"` / `"Edit"` literally, so an
  aliased write is not recorded in the change-ledger's `TouchedFiles`.
  That is a tracking surface, not a security control, so it was left for a
  separate issue.
