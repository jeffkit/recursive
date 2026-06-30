# tui-acceptance — AI-driven TUI testing workflow for Recursive

> Status: canonical SOP. Loaded by reference from TUI goal files'
> "Notes for the agent" section so the self-improve loop follows it
> automatically whenever a goal touches `crates/recursive-tui/`.

## When to follow

Apply this workflow to **every** goal that changes behaviour in
`crates/recursive-tui/` (src or tests). Trigger: the goal's "Touches"
line lists any path under `crates/recursive-tui/`.

Do NOT apply for: pure doc changes, non-TUI crates, roadmap/admin edits.

## The three layers (use all three)

| Layer | Tool | What it catches | Cost |
|---|---|---|---|
| Logic + render | in-process `Harness` (`crates/recursive-tui/src/harness.rs`, `#[cfg(test)]`) | state-machine + off-screen rendering regressions | ms |
| Effectiveness | `cargo-mutants` via `.dev/scripts/tui-mutants.sh` | tests that pass but don't pin behaviour | minutes |
| Integration | `tui-pty` binary (`cargo run -p tui-pty-harness`) | real-terminal IO the in-process harness can't reach | seconds |

Stages 1–4 built these; this doc is how to drive them on a new change.

## SOP

### 1. Write the test at the rendered layer first

Before/while implementing, add a test in `crates/recursive-tui/src/<module>.rs`
under `#[cfg(test)]` that drives the change through `Harness`:

```rust
use crate::harness::Harness;
use crate::events::UiEvent;

let mut h = Harness::new();
h.pump(UiEvent::AssistantMessage { content: "…".into() });
let screen = h.render();
assert!(screen.find_row("…").is_some(), "{}", screen.numbered());
```

Prefer **visual** assertions (`Screen::find_row`, `row_has_bg_color`,
`text()`, `numbered()`) over internal-state peeks — they catch what the
user actually sees. For highlight/marker alignment, assert on the
specific colour (`Screen::bg` / `row_has_bg_color`), not "any bg"
(panel blocks fill every row with a base colour).

### 2. Run the in-process suite

```bash
cargo test -p recursive-tui            # dev-dep enables test-utils; no flag needed
```

All green before proceeding.

### 2.5. Presence check — did you actually add a test? (enforced, fast)

```bash
.dev/scripts/tui-test-presence.sh
```

Exit 0 if no `crates/recursive-tui/src/` file changed, OR a test-bearing
change was detected (a new `#[test]` / `#[cfg(test)]` / `mod tests` in a
changed src file, a change under `crates/recursive-tui/tests/`, or a
`tui-pty-harness` change). Exit 1 if TUI src changed with no test
addition — fix by writing the test (step 1), not by opting out. Set
`RECURSIVE_TUI_TEST_PRESENCE=0` only for a pure refactor with no
behaviour change, and document why in the journal. This runs as the
flow `tui-presence` gate BEFORE `tui-mutants`, so the cheap "you forgot
tests" case is caught in milliseconds instead of a mutation-gate
resume-fix cycle.

### 3. Prove the tests bite — mutation gate (enforced)

```bash
.dev/scripts/tui-mutants.sh                       # auto: files changed vs main
.dev/scripts/tui-mutants.sh crates/recursive-tui/src/app/render.rs
```

- **Exit 0** (no survivors in the touched files) → tests are effective.
- **Exit non-zero** (survivors) → read `mutants.out/missed.txt`, add or
  strengthen the harness test that should have caught each survivor,
  re-run until green.

**This step is a hard gate in the self-improve flow** (since the tui-test
review): the flow's `tui-mutants` project gate — declared in
`.flowcast/gates.json` and loaded via `mergeGates` — runs
`tui-mutants.sh` after the e2e gate when a goal changes anything under
`crates/recursive-tui/src/`. It uses `onFail: resume-fix`: the flow feeds
the survivor report back to the agent to strengthen tests, then re-runs
the gate; still failing → rollback (same shape as clippy/e2e).
`cargo-mutants` missing is also a hard failure — install it
(`cargo install cargo-mutants`). `tui-mutants.sh` self-skips (exit 0)
when no TUI source changed, so non-TUI goals pay nothing. So step 3 is
no longer advisory; an agent that skips it cannot land weak TUI tests.
The legacy `.dev/scripts/self-improve.sh` is deprecated and does NOT
carry this gate — use the flow (`.dev/flows/self-improve.flow.js`).

Scope to the **touched files only** (the default). A whole-crate run is
slow and out of scope for a single change. Exceptions: survivors in
`lib.rs` terminal-IO code (raw mode, mouse, alternate screen) are
expected — that layer is covered by step 4, not the in-process harness.
Document such expected survivors in the goal's Notes rather than
chasing them in-process.

### 4. PTY-tour the real binary for acceptance

For changes visible at startup, in a panel, or in input handling, run
the actual binary under the PTY harness and read the screen:

```bash
cargo build -p recursive-tui
cargo run -q -p tui-pty-harness -- run \
  --bin "$PWD/target/debug/recursive-tui" \
  --keys "hello\r" --wait-ms 2500 --snap numbered
```

Keys grammar: `\r` `\n` `\t` `\e`(=ESC) `\xNN` `^x`(=Ctrl+x). Use
`--snap json` when the AI needs to parse the screen programmatically.
`--bin` takes an **absolute** path (the PTY spawner does not resolve
relative paths).

Assert (in the goal's Acceptance) the specific screen content the user
should see after the key script, e.g. "splash shows /resume hint" or
"typing `/theme\r↑\r` lands the highlight bar on the `▶` row".

### 5. Quality gates (mandatory, CLAUDE.md)

```bash
cargo fmt --all --check
cargo clippy -p recursive-tui --all-targets -- -D warnings
cargo test -p recursive-tui
```

The workspace-wide `cargo clippy --all-features` is pre-existing red on
`recursive-cli` (undeclared `web_search`/`http` cfg) — unrelated; do not
block on it unless your change is in `recursive-cli`.

## Anti-patterns

- ❌ Asserting only on `App` internal fields when a `Screen::find_row`
  assertion would catch the rendering too.
- ❌ Using a "any background" check to detect a highlight bar — panel
  base fills make every row "have bg". The coarse `Screen::has_bg` /
  `row_has_bg` helpers were removed for this reason; use `row_has_bg_color`
  with the specific highlight colour (or `row_has_bg_other_than` with the
  panel's base colour to filter it out).
- ❌ Skipping step 3 because step 2 passed — passing tests can be
  tautologies; the mutation gate is the effectiveness check (and a hard
  gate in the self-improve flow via `.flowcast/gates.json`).
- ❌ PTY-tour with a relative `--bin` path — the spawner won't find it.
- ❌ Mutating the whole crate every commit — scope to touched files.
- ❌ Sleeping a fixed `--wait-ms` and hoping the TUI finished — the PTY
  harness now polls for screen stability (`--stable-ms`). Prefer the
  `tui_pty_harness` lib in tests over shelling out, and set `--stable-ms`
  high enough for slow CI boots.
