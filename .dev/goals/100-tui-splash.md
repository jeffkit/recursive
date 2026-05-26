# Goal 100 — TUI: Logo + Splash Screen

**Roadmap**: Phase 11.4 — TUI (part 4/5)

**Design principle check**:
- Implemented as: splash screen state in `crates/recursive-tui/src/main.rs`
- ❌ Does NOT modify core library
- Visual/UX polish only

## Why

A splash screen with the Recursive logo gives the TUI a polished,
professional feel. It shows on startup for 1-2 seconds or until the
user presses any key, then transitions to the main conversation view.

## Scope (do exactly this, no more)

### 1. ASCII art logo

```
╭─────────────────────────────────────╮
│                                     │
│   ╱╲    Recursive Agent            │
│  ╱  ╲   ─────────────────          │
│ ╱ ╱╲ ╲  v0.4.0                     │
│ ╲ ╲╱ ╱                             │
│  ╲  ╱   Self-improving AI agent    │
│   ╲╱    in Rust                    │
│                                     │
│   Press any key to continue...      │
│                                     │
╰─────────────────────────────────────╯
```

(Feel free to adjust the exact art — the key is: recognizable logo,
version number, tagline, "press any key" prompt.)

### 2. App state machine

```rust
enum AppState {
    Splash,
    Chat,
}
```

- Start in `Splash` state
- On any keypress OR after 2 seconds → transition to `Chat`
- `Chat` state = current behavior (messages + input)

### 3. Splash screen rendering

- Center the logo block vertically and horizontally in the terminal
- Use a distinct color scheme (e.g., cyan for the logo art, white for text)
- Show version from `env!("CARGO_PKG_VERSION")` or hardcoded "0.4.0"

### 4. Tests

- Test: App starts in Splash state
- Test: any keypress transitions to Chat state
- Test: Chat state behaves as before (existing tests still pass)

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean
- TUI shows splash screen on startup

## Notes for the agent

- Read `crates/recursive-tui/src/main.rs` for current App struct.
- Add `state: AppState` field to App.
- In the main loop, dispatch rendering based on state (splash vs chat).
- In handle_key, if state is Splash, any key → transition to Chat.
- For the 2-second auto-transition, track a start time and check elapsed.
- **DO NOT modify any file in `src/`.**
- **Keep it simple — the splash is purely cosmetic.**
