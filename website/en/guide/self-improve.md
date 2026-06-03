# Self-Improving Agents

One of Recursive's most distinctive features is that it runs its own development loop. The same agent kernel you use to build your own tools is the one that implements new features in Recursive itself.

## How it works

The self-improvement loop lives in `.dev/scripts/self-improve.sh`. At a high level:

```
1. Read goal from .dev/goals/ or .dev/ROADMAP.md
2. Launch recursive loop with coding tools (read_file, write_file, apply_patch, run_shell)
3. Agent reads the codebase, understands the goal, makes changes
4. Run `cargo test` to verify
5. Run `cargo clippy` to check quality
6. If all pass: commit the changes
7. If fail: rollback, try again with adjusted approach
8. Emit an observation to .dev/journal/ for the next run
```

## The observation system

After each run, the agent writes a journal entry to `.dev/journal/`. These entries contain:
- What was attempted
- What succeeded or failed
- Lessons learned for next time

On the *next* run, the agent reads recent journal entries before starting. This creates a persistent feedback loop — the agent learns from its mistakes without any external training.

## Key invariants

The self-improve loop enforces several invariants documented in `.dev/AGENTS.md`:

| Invariant | Description |
|---|---|
| #1 | Agent loop stays small — new capabilities go into tools, not the loop |
| #3 | Sandbox — all fs/shell tools use `resolve_within` |
| #5 | No `unwrap()` in product code |
| #8 | Tool-call ↔ tool-result pairing preserved |

These invariants are *checked in code* — clippy and tests enforce them, not documentation.

## Using the loop for your own project

You can use the same pattern for any codebase:

1. Create a `.dev/goals/` directory with goal files
2. Add an `AGENTS.md` at the project root describing invariants, conventions, and context
3. Run `recursive loop --workspace . "read .dev/goals/ and implement the next unfinished goal"`

```bash
# Create a goal
cat > .dev/goals/01-add-caching.md << 'EOF'
## Goal: Add in-memory caching to the API layer

The /api/users endpoint is slow because it queries the DB on every request.
Add a simple TTL cache (5 minutes) using a HashMap wrapped in RwLock.

Acceptance criteria:
- Cache hit ratio > 80% in load test
- No data races (use Arc<RwLock<...>>)
- Cache invalidated on write operations
EOF

# Run the loop
recursive loop "read .dev/goals/ and implement the next unfinished goal"
```

## The `apply_patch` discipline

One metric the observation system tracks is the **`apply_patch` : `write_file` ratio**.

- High ratio = agent makes surgical edits → good
- Low ratio = agent kept failing `apply_patch` and fell back to rewriting files → indicates poor anchoring in patches

When `apply_patch` fails (ambiguous context lines), the correct response is to widen the anchor — not to fall back to `write_file`.

## Monitoring a run

Watch the `StepEvent` stream to monitor what the agent is doing:

```rust
agent.builder()
    .on_event(|e| match e {
        StepEvent::ToolStart { name, args, .. } => {
            if name == "apply_patch" {
                println!("📝 Patching: {}", args["path"]);
            } else if name == "run_shell" {
                println!("🔧 Shell: {}", args["command"]);
            }
        }
        StepEvent::Done { finish_reason, .. } => {
            println!("✅ Done: {:?}", finish_reason);
        }
        _ => {}
    })
```
