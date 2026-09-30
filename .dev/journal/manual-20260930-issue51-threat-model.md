# Manual change — issue #51: sandbox threat model + microvm egress gate + env invariant

- **Date**: 2026-09-30
- **Goal**: Document per-tier "defends against / does not defend against",
  align the microvm tier's egress story with the container tier's
  default-off opt-in (`RECURSIVE_SANDBOX_NETWORK`), and declare + test the
  sandbox env non-inheritance invariant (issue #51).

## Files touched

- `docs/architecture/execution-environments.md` — fixed two stale
  `RECURSIVE_CONTAINER_NETWORK` → `RECURSIVE_SANDBOX_NETWORK`; added the
  top egress warning; new "Threat Model" section (per-tier defenders,
  host-side tools inside/outside the sandbox boundary, env invariant);
  updated capability/isolation matrix network rows; next-milestone note
  about host-side tools as a second known gap.
- `src/tools/e2b_provider.rs` — `E2bConfig::from_env` now refuses the
  default `base` template unless `RECURSIVE_SANDBOX_NETWORK=on` (custom
  templates own their egress decision); `default_caps(&template_id)`
  honestly reports `network=false` for base-without-opt-in; module header
  documents the gate. **Behavior change**: existing microvm users must
  now set `RECURSIVE_SANDBOX_NETWORK=on` (or a custom template).
- `tests/issue51_sandbox_env_inheritance.rs` — promoted from the
  investigation-phase `wip-` file (renamed, doc header updated): host env
  never reaches the transport env slice; explicit env arg is the only
  channel; LocalTransport positive control.
- `.dev/AGENTS.md` — invariant #3 extension declaring env
  non-inheritance with the new regression-test anchor.

## Tests added

- `e2b_config_from_env_egress_gate` (merged into
  `e2b_config_from_env_defaults_and_overrides` per the one-env-var-test
  rule): base+no opt-in → `Err` mentioning `RECURSIVE_SANDBOX_NETWORK=on`;
  base+on → `Ok`; custom template → `Ok` (asserted **without** the
  opt-in var, so the gate-bypass branch is genuinely exercised);
  capabilities `network=false`/`true` for the two base cases.
- `tests/issue51_sandbox_env_inheritance.rs` (single sequential test).

## Notes

- The env-var tests touch `RECURSIVE_SANDBOX_NETWORK`; all checks were
  collapsed into one sequential test per `.dev/AGENTS.md` ("Env-var tests
  must be ONE test") to avoid the parallel set_var race.
- E2B is managed: the host cannot disable the VM NIC, so "default-off
  egress" is implemented as a startup acknowledgment gate, not isolation —
  stated plainly in the doc warning.
- Commit message body must state (per plan §4): the new e2b unit tests
  live behind the non-default `e2b-sandbox` feature (not run by plain
  `cargo test --workspace`, only compiled by `clippy --all-features`),
  and that microvm now refusing to start without
  `RECURSIVE_SANDBOX_NETWORK=on` (base template) is a breaking behavior
  change for existing microvm users.
