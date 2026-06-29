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

### 3. Prove the tests bite — mutation gate

```bash
.dev/scripts/tui-mutants.sh                       # auto: files changed vs main
.dev/scripts/tui-mutants.sh crates/recursive-tui/src/app/render.rs
```

- **Exit 0** (no survivors in the touched files) → tests are effective.
- **Exit non-zero** (survivors) → read `mutants.out/missed.txt`, add or
  strengthen the harness test that should have caught each survivor,
  re-run until green.

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
- ❌ Using `Screen::has_bg` / `row_has_bg` to detect a highlight bar —
  panel base fills make every row "have bg". Use `row_has_bg_color` with
  the specific highlight colour.
- ❌ Skipping step 3 because step 2 passed — passing tests can be
  tautologies; the mutation gate is the effectiveness check.
- ❌ PTY-tour with a relative `--bin` path — the spawner won't find it.
- ❌ Mutating the whole crate every commit — scope to touched files.
