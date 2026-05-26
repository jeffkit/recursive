# Goal 97 — TUI: Crate Scaffold + Basic REPL Display

**Roadmap**: Phase 11.1 — TUI (part 1/5)

**Design principle check**:
- Implemented as: new crate `crates/recursive-tui/` in a Cargo workspace
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- TUI communicates via the library API, not direct agent internals
- Orthogonal: TUI is a consumer of the recursive library

## Why

A terminal UI makes Recursive interactive and visual — showing tool calls,
streaming output, and conversation history in real-time. The first step
is scaffolding: set up the workspace, create the TUI crate with ratatui,
and render a basic REPL-like display (input area + message history).

## Scope (do exactly this, no more)

### 1. Convert to Cargo workspace

Transform the root `Cargo.toml` into a workspace:

```toml
[workspace]
members = ["crates/recursive-tui"]

[package]
# ... existing package fields stay unchanged ...
```

Move nothing — the existing crate stays at the root. Only add the
workspace section. The root crate is implicitly a workspace member.

### 2. Create `crates/recursive-tui/Cargo.toml`

```toml
[package]
name = "recursive-tui"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "recursive-tui"
path = "src/main.rs"

[dependencies]
recursive-agent = { path = "../.." }
ratatui = "0.29"
crossterm = "0.28"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

### 3. Create `crates/recursive-tui/src/main.rs`

A minimal ratatui application that:
1. Initializes the terminal (crossterm backend)
2. Shows a two-panel layout:
   - Top: message history area (scrollable text)
   - Bottom: input line
3. Handles basic keyboard input:
   - Type characters → appear in input line
   - Enter → move input text to message history as "You: <text>"
   - `q` or Ctrl+C → quit
4. Restores terminal on exit

This is a LOCAL-ONLY UI — it does NOT connect to the agent yet.
Just proves the ratatui setup works.

```rust
// Pseudo-structure:
fn main() -> Result<()> {
    let terminal = setup_terminal()?;
    let app = App::new();
    run_app(terminal, app)?;
    restore_terminal()?;
    Ok(())
}

struct App {
    input: String,
    messages: Vec<String>,
    should_quit: bool,
}
```

### 4. Tests

In `crates/recursive-tui/src/main.rs` or a `tests/` dir:
- Test: App::new() creates empty state
- Test: handling Enter moves input to messages
- Test: handling 'q' sets should_quit

(Unit tests only — no terminal required)

## Acceptance

- `cargo build -p recursive-tui` compiles
- `cargo test` (root workspace) passes all existing tests + new TUI tests
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- Running `cargo run -p recursive-tui` shows a terminal UI (manual check)

## Notes for the agent

- Read root `Cargo.toml` for the existing package structure.
- The workspace `members` field should be `["crates/recursive-tui"]`.
  The root package is an implicit workspace member.
- Use `ratatui 0.29` (latest stable) with `crossterm` backend.
- The TUI app is standalone — it imports `recursive-agent` as a dependency
  but does NOT use it yet in this goal. Just prove the crate compiles
  with the dep available.
- For the layout, use `ratatui::layout::{Layout, Constraint, Direction}`.
- For the input, handle `crossterm::event::{Event, KeyCode}`.
- **DO NOT modify any existing source files except root Cargo.toml (workspace section only).**
- **DO NOT connect to the agent or HTTP API yet — that's future goals.**
- **Keep it simple: ~150-200 lines total for the TUI scaffold.**
