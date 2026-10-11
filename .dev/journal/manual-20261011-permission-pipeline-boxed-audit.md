# Manual change — 2026-10-11

Date: 2026-10-11
Goal: unblock the `feature-matrix-bare` gate (`cargo clippy --lib --no-default-features -- -D warnings`)
for pipeline-158.

Files touched:
- `src/tools/policy_domain/permission_pipeline.rs` — `CheckOutcome::Deny` now carries
  `audit: Box<AuditMeta>` (12 construction sites); enum-level doc explains why.
- `src/tools/dispatch.rs` — `invoke_with_audit` unboxes: `audit: *audit`.

Tests added: none (the failure is a layout lint; the lint itself is the check).

Notes:
- Root cause: `AuditMeta` is ~150 bytes and `Error` ~96 bytes, so the `Deny` variant was
  ~248 bytes against `Allow`'s 32 — `clippy::large_enum_variant` (threshold = 200-byte
  *difference*) fires. It only fired under `--no-default-features`; the `--all-features`
  config passed. `AuditMeta` grew when #153 added `tool_name` + `canonical_name`, which
  pushed the variant over the threshold.
- Fix follows clippy's own suggestion (box the large field) rather than an `#[allow]`;
  no check was silenced or weakened. Denials are rare, so the allocation is off the hot path.
- Verified: `cargo fmt --all --check`, `cargo clippy --lib --no-default-features -- -D warnings`,
  and the full gate runner (`gates=6 … ALL GATES PASSED`).
