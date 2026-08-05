# Goal 386 — Bare `recursive` launches the TUI (align install / comment / reality)

**Roadmap**: UX — default entry. `install.sh` tells users
`recursive  # open TUI`, and `main.rs` comments say "Nothing → TUI (if
compiled in), else REPL", but the code always selects `Cmd::Repl` (a
line-oriented `recursive>` prompt with no todo panel / command palette /
plan modal). Homebrew installs only the `recursive` binary — users never
reach `recursive-tui`.

**Design principle check**:
- Implemented as: wire the CLI default path to the TUI crate (dependency
  + launch), keep `recursive repl` as the explicit line-oriented REPL.
- ❌ Does NOT change the agent kernel / `run_inner`.
- ❌ Does NOT remove the REPL — only stop making it the silent default.

## Why (verified 2026-08-04)

1. **`crates/recursive-cli/src/main.rs:615-636`** — comment claims TUI
   default; `else` branch is `Cmd::Repl`.
2. **`crates/recursive-cli/src/main.rs:2436-2474`** — `repl()` is a
   stdin line loop (`recursive>`), explicitly notes plan-mode is "use TUI
   or HTTP".
3. **`install.sh:131`** — prints `recursive  # open TUI`.
4. **`docs/homebrew/recursive.rb`** — `bin.install "recursive"` only;
   no `recursive-tui`.
5. **`crates/recursive-cli/Cargo.toml`** — no dependency on
   `recursive-tui`.
6. Self-improve has invested heavily in TUI (Goals 343–384) while the
   published default path bypasses it — product/investment mismatch.

## Scope (do exactly this, no more)

### 1. Depend on and launch the TUI from the CLI default path

- Add `recursive-tui` as a dependency of `recursive-cli` (path dependency).
- Extract a small, pure command-resolution helper so dispatch policy is
  testable without starting a terminal.
- Preserve all existing precedence: explicit subcommand, `-r/--resume`,
  `-c/--continue`, and `-p <prompt>` keep their current paths. Explicit
  `recursive repl` keeps the line REPL.
- Bare invocation starts the TUI only when stdin/stdout are suitable for an
  interactive terminal. For a non-TTY invocation, prefer a clear error that
  points to `recursive repl` (or preserve a REPL fallback if that matches an
  existing documented pipe contract). Whichever policy is chosen must be
  explicit, tested, and recorded in the journal; never attempt raw mode on a
  non-TTY silently.
- For the interactive bare case, call the existing public
  `recursive_tui::run() -> std::io::Result<()>` entrypoint.
- Audit the existing `--weixin` path: it uses the TUI's
  `run_with_backend(backend)` seam. The new default branch must not bypass or
  replace that backend construction. Add a dispatch regression test if the
  branch shares command-resolution code.

### 2. Update clap / help copy

- Change the `Repl` variant doc from "default when no command is given"
  to something accurate (e.g. "Line-oriented multi-turn REPL (explicit;
  bare `recursive` opens the TUI)").
- Fix the comment at lines 610–616 to match the new behaviour.

### 3. Packaging

- Ensure release / install paths that currently only ship `recursive`
  still work: TUI code is **linked into** the `recursive` binary, so a
  separate `recursive-tui` binary is optional. Do **not** require users
  to install a second binary for the default path.
- Update `install.sh` only if the printed help lines need a one-line
  tweak (e.g. mention `recursive repl` for the line REPL). Prefer
  minimal edit.
- Homebrew formula: no change required if TUI is inside `recursive`;
  if the formula builds workspace members, confirm `recursive` still
  builds with the new dep.

### 4. Tests

- Unit-test the pure dispatch helper with at least these cases:
  - bare interactive invocation → TUI;
  - bare non-TTY invocation → the documented error or REPL fallback;
  - `-p "goal"` → one-shot Run;
  - `-c` → resume latest;
  - `-r <id>` → Resume;
  - explicit `recursive repl` → Repl.
- Add a regression assertion that an existing `--weixin` launch still uses
  its pre-constructed backend path rather than bare `recursive_tui::run()`.
- Existing REPL tests / behaviours must remain reachable via
  `recursive repl`.
- `cargo test -p recursive-cli` and `cargo test -p recursive-tui` green.

## Files NOT to touch

- `src/run_core.rs`, `src/kernel.rs`, `src/runtime.rs` (except if TUI
  already imports them — no new kernel logic).
- ACP / weixin / HTTP handlers.
- Broad TUI redesign — launch only.

## Acceptance

- `cargo build -p recursive-cli` links TUI and produces `recursive`.
- Dispatch-helper tests prove the six cases in Scope §4, including non-TTY
  behaviour and preservation of `-p` / `-c` / `-r` / explicit `repl`.
- Manual journal smoke checks:
  - interactive bare `recursive` opens the TUI splash/chat;
  - a non-TTY invocation follows the documented policy without entering raw
    mode;
  - `recursive repl` still prints `recursive>` and accepts `:q`.
- The existing `--weixin` path still reaches `run_with_backend` semantics.
- `cargo test --workspace` / clippy `-D warnings` / fmt clean.
- Grep: the misleading comment "Nothing → TUI (if compiled in), else REPL"
  is gone or corrected to match code.
- Grep: `Cmd::Repl` is no longer the unconditional `else` of the
  no-subcommand branch.
- Journal: `.dev/journal/manual-20260804-goal386-default-tui.md`.

## Notes for the agent (traps)

- **Config validation**: compare the existing REPL and standalone TUI
  bootstrap paths, then keep validation at one appropriate boundary. Do not
  assume the TUI already calls `config.validate_for_agent()`, and do not add
  duplicate validation with different errors.
- **Feature flags**: `recursive-cli` enables `http` by default; TUI may
  talk to an in-process runtime rather than HTTP — follow the existing
  `recursive-tui` architecture (`Local` backend vs HTTP). Do not force
  an HTTP server on every interactive start unless that is already how
  `recursive-tui` works.
- **Binary size**: linking ratatui into `recursive` is acceptable; do not
  add a new feature flag maze unless compile time becomes extreme —
  document in journal if you introduce `feature = "tui"` default-on.
- If `recursive_tui::run` needs `std::io::Result` vs `anyhow`, map errors
  at the CLI boundary with a single `?` / `map_err` — no redesign.
- Windows/macOS terminal: no change to raw-mode logic in this goal.
