# Manual journal — Goal 405 microVM tier: E2B ToolTransport + wiring (issue #32)

## Date
2026-09-29

## Goal
Wire the microVM sandbox tier end-to-end: `E2bTransport` implementing the
full `ToolTransport` contract (read/write/list/walk/exec/capabilities/
destroy + TTL renewal), `E2bToolSetProvider::build_registry` rebinding the
full standard toolset through it, CLI builder `SandboxMode::MicroVm`
branch, `e2b-sandbox` forwarding feature, and the
`docs/architecture/execution-environments.md` design doc.

## Files touched
- `src/tools/e2b_provider.rs` — rewritten: `E2bTransport` (lazy sandbox,
  `pwd`/`whoami`/toolchain probes cached into `capabilities()`,
  single-round-trip `find`-based `walk`, 404→`NotFound` mapping,
  half-TTL renewal with surfaced errors, idempotent `destroy`), provider
  `build_registry` now mirrors the container tier
  (`block_in_place` + `build_standard_tools_with_transport_opt`,
  `disable_host_exec: true`), old `E2bShellTool` removed.
- `src/tools/mod.rs` — re-export `E2bToolSetProvider` behind the feature.
- `crates/recursive-cli/Cargo.toml` — `e2b-sandbox` forwarding feature.
- `crates/recursive-cli/src/cli/builder.rs` — real `MicroVm` branch
  (feature on: from_env → provider → registry; feature off / missing key:
  clear error + exit 2, no silent fallback).
- `docs/architecture/execution-environments.md` — new (tier matrix,
  capability matrix, microVM selection criteria, non-goals).
- `docs/architecture/index.md` — link added.
- `README.md` — env table fixed (`RECURSIVE_SANDBOX=microvm`, TTL 3600).

## Tests added
`src/tools/e2b_provider.rs` unit tests (no network):
`e2b_config_from_env_defaults_and_overrides`, `http_status_classification`,
`shell_quote_escapes_single_quotes`, `parse_ls_line_distinguishes_dirs`,
`parse_walk_line_is_root_relative`, `cap_string_truncates_with_marker`,
`e2b_transport_default_capabilities_are_conservative`,
`e2b_transport_debug_has_no_secrets`; live round-trip
`e2b_shell_exec_runs_against_live_api` gated on
`RECURSIVE_TEST_E2B_API_KEY` (skip-if-absent).

## Notes
- **Live E2B round-trip did NOT run**: no `RECURSIVE_TEST_E2B_API_KEY` in
  this environment — the live test skipped. The no-key / no-feature CLI
  paths were verified by hand (both exit 2 with the intended messages).
- Directed verification: `cargo test -p recursive-agent --features
  e2b-sandbox e2b` → 10 passed; `cargo build -p recursive-cli --features
  e2b-sandbox` clean; `cargo clippy -p recursive-agent/-p recursive-cli
  --all-targets --features e2b-sandbox -- -D warnings` clean;
  `cargo clippy -p recursive-cli --all-targets --all-features -- -D
  warnings` clean; `cargo fmt --all` applied.
- The full `repo-tests.sh` gate is intentionally NOT run here (pipeline
  has an independent quality gate, per dispatch instructions).

## Reviewer fixes (2026-09-29, same day)
- **Host→VM path mapping added (blocker)**: `E2bTransport` now takes the
  host `workspace`; `map_path` (mirrors `ContainerTransport::map_path`)
  rewrites host workspace paths to a fixed `/workspace` VM root for
  read/write/list/walk/create_dir_all and `exec_shell`'s cwd. The root is
  created at sandbox start; `capabilities().path_root` is `/workspace`
  (not `pwd`). The VM starts **empty** (no host pre-upload) — documented
  in code + `execution-environments.md`. New test
  `map_path_translates_workspace_prefix`; live test updated to exec via
  a mapped host cwd.
- **Drop no longer bare `tokio::spawn`**: mirrors
  `ContainerTransport::drop` — `Handle::try_current()` first, blocking
  current-thread runtime fallback when outside a runtime.
- **reqwest client timeout**: global 30s `timeout` removed (it truncated
  transport-level 60/120s+ exec timeouts); replaced by a 30s
  `connect_timeout` only.
- **Docs**: added isolation-dimension table, density/cold-start table +
  why the default tier must stay `none`, self-hosted microVM requirements
  checklist, path-semantics section, non-goal for workspace pre-upload,
  and next-milestone list (egress policy, credential broker, audit,
  per-session HTTP sandboxing).
