# Goal 101 — TUI: Plan Mode UI (approve/reject)

**Roadmap**: Phase 11.5 — TUI (part 5/5)

**Design principle check**:
- Implemented as: Plan mode state in `crates/recursive-tui/src/main.rs`
- ❌ Does NOT modify core library
- TUI-only feature leveraging existing PlanningMode in the agent

## Why

When the agent enters planning mode (via PlanningMode::PlanFirst), it
proposes a plan before executing. The TUI needs to display the plan and
let the user approve (Enter/y) or reject (n/Esc) it. This closes the
interactive loop for plan-based workflows.

## Scope (do exactly this, no more)

### 1. Plan mode detection

When the agent response includes a plan (detected by checking message
content for plan markers or a separate API field), the TUI enters plan
display mode.

For this implementation, treat any assistant message starting with
"Plan:" or "## Plan" as a plan proposal.

### 2. Plan display UI

When a plan is detected, switch to a plan review screen:

```
┌─ Plan Proposal ───────────────────────────────────┐
│                                                    │
│  ## Plan                                          │
│  1. Read the configuration file                   │
│  2. Modify the timeout setting                    │
│  3. Run tests to verify                           │
│                                                    │
│                                                    │
└────────────────────────────────────────────────────┘
  [Enter/y] Approve    [n/Esc] Reject    [e] Edit
┌─ Input ───────────────────────────────────────────┐
│ ▌                                                  │
└────────────────────────────────────────────────────┘
```

### 3. App state extension

```rust
enum AppScreen {
    Splash,
    Chat,
    PlanReview { plan_text: String },
}
```

### 4. Plan actions

- **Approve** (Enter or 'y'): send "approved" message to session, return to Chat
- **Reject** (Esc or 'n'): send "rejected" message to session, return to Chat
- **Edit** ('e'): return to Chat with plan text pre-filled in input for editing

### 5. Tests

- Test: plan message triggers PlanReview state
- Test: approve sends "approved" and returns to Chat
- Test: reject sends "rejected" and returns to Chat
- Test: edit pre-fills input and returns to Chat

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean
- Plan mode shows plan text and responds to approve/reject keys

## Notes for the agent

- Read `crates/recursive-tui/src/main.rs` for current AppScreen enum and state management.
- Plan detection is simple string matching — no need to parse structured data.
- The plan review screen uses the same layout but replaces the messages panel content
  with just the plan text, and shows action keys in the status bar.
- For "send message" on approve/reject, use the same `send_message_to_session`
  pattern (spawn tokio task with reqwest).
- **DO NOT modify any file in `src/`.**
- **Keep plan detection simple — just check if content starts with "Plan:" or "## Plan".**
