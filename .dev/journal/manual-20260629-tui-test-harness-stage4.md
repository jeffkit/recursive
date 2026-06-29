# Manual edit: tui-test-harness stage 4 (tui-pty integration harness)

**Date**: 2026-06-29
**Goal**: The integration-layer "eyes" — run the **real** `recursive-tui`
binary under a PTY and snapshot its screen, replicating the Wrap-terminal
capability the user admired. Covers exactly the `recursive-tui` `lib.rs`
terminal-IO layer (raw mode, alternate screen, mouse) that stages 1–3
cannot reach in-process.

**Files touched**:
- `Cargo.toml` (workspace) — add `crates/tui-pty-harness` member.
- `crates/tui-pty-harness/Cargo.toml` (new) — `portable-pty`, `vt100`,
  `serde_json`, `anyhow`.
- `crates/tui-pty-harness/src/main.rs` (new) — the `tui-pty` binary.

**New dependencies (justification, invariant #6)**:
- `portable-pty 0.8` — wezterm's cross-platform PTY. Implementing PTY
  allocation by hand across macOS/Linux/Windows is infeasible; this is the
  de-facto choice. Lives only in the new harness crate, never in the
  product crates.
- `vt100 0.15` — a correct VT100/ANSI terminal state model with a readable
  `Screen` grid. Reimplements the terminal escape handling the real TUI
  targets, so the snapshot reflects what a user's terminal shows.
- `serde_json`, `anyhow` — already used elsewhere in the workspace; JSON
  snapshot output and ergonomic error handling.

**Design**:
- Single-shot `run` subcommand: spawn the binary under a PTY, type an
  optional key script, wait, snapshot the vt100 screen, print, tear down.
- `--keys` mini-grammar: `\r` `\n` `\t` `\e`(=ESC) `\xNN` `^x`(=Ctrl+x,
  `^[`=ESC). UTF-8 literals preserved.
- `--snap text|numbered|json`.
- Reader thread drains PTY bytes into a shared `Mutex<vt100::Parser>`; main
  thread sleeps `--wait-ms`, snapshots, then kills the child (→ reader
  EOF) and joins. Slave end is dropped after spawn so the master sees EOF
  on child exit.
- Refactored to `RunSpec` to keep clippy's `too_many_arguments` happy.
- `publish = false` — this is dev/CI tooling, not a published crate.

**Stateful daemon (Wrap-style spawn/type/snap across invocations) is
deliberately out of scope** for this stage; the single-shot form already
lets an AI observe running state for acceptance tours. Noted as future
work.

**Tests** (5, all pass):
- `parse_keys_literals_are_utf8_safe`, `parse_keys_escape_sequences`,
  `parse_keys_ctrl_sequences` — the key grammar.
- `shell_split_handles_quotes_and_whitespace` — the `--bin` splitter.
- `spawn_and_snapshot_captures_child_output` — **real PTY smoke test**:
  spawns `echo hello-pty` under a PTY and asserts the snapshot contains it.
  Proves the portable-pty + vt100 pipeline end-to-end.

**End-to-end demo** (in the worktree):
```
cargo run -q -p tui-pty-harness -- run --bin "$PWD/target/debug/recursive-tui" \
  --wait-ms 2500 --snap numbered
```
Captured the real `recursive-tui` splash: the Recursive box-logo, version
`v0.7.0`, model `deepseek-v4-flash`, "Type a message to start" /
"/resume to continue a session · /help for commands", the status bar, and
the bordered input box with the `❯` prompt. This is the AI reading the
live TUI's screen through a real terminal.

**Quality gates** (in `.worktrees/feat-tui-test-harness`):
- `cargo fmt --all --check` — clean
- `cargo clippy -p tui-pty-harness --all-targets -- -D warnings` — clean
- `cargo test -p tui-pty-harness` — 5 passed, 0 failed

**Next**: stage 5 wires stages 1–4 into a `/recursive-loop`-style skill +
goal template so self-improve automatically writes harness tests, runs the
mutation gate, and PTY-tours the real binary for acceptance.
