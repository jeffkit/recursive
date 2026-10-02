# Issue #69 — RECURSIVE_ALLOW_TOOLS had no effect on `recursive http`

## Date
2025 (manual fix)

## Goal
`RECURSIVE_ALLOW_TOOLS` was documented (src/config.rs field doc) but never read
by `Config::from_env`; only the `--allow-tools` CLI flag populated
`config.allow_tools` (crates/recursive-cli/src/main.rs). The HTTP entry does
call `tools.retain_tools(&config.allow_tools)` (main.rs, Cmd::Http branch), but
the list was always empty when only the env var was set — so `GET /tools`
returned all 26 tools instead of the narrowed set.

## Files touched
- `src/config.rs` — `Config::from_env` now reads `RECURSIVE_ALLOW_TOOLS`
  (comma-split, trimmed, blank entries dropped; blank/absent = no narrowing).
- `src/config.rs` — new test `config::tests::allow_tools_from_env`.

## Tests added
`allow_tools_from_env`: parses `Read, Write ,Bash` → `["Read","Write","Bash"]`;
blank value → empty (no narrowing). Env-lock + PinnedRecursiveHome guarded per
house style.

## Verification
- `cargo test --lib config::` 86 passed; `http::` 85 passed.
- `cargo clippy --all-targets --all-features -- -D warnings` clean; `cargo fmt` clean.
- Live repro from the issue: `RECURSIVE_ALLOW_TOOLS=Skill,HttpCall` +
  `recursive http` → `GET /tools` returns 1 tool (`Skill`; `HttpCall` is #63,
  not yet implemented). Expected per issue: 1.

## Notes
CLI flag precedence is unchanged: main.rs overwrites `config.allow_tools` when
`--allow-tools` is present, so flag still wins over env. Same problem class as
#65 (re-filed to avoid the false-positive body).
