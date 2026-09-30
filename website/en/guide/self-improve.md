# Self-Improving Agents

One of Recursive's most distinctive features is that it runs its own development loop. The same agent kernel you use to build your own tools is the one that implements new features in Recursive itself.

## How it works

The self-improvement loop runs on the **plaita engine**:

```bash
.dev/scripts/launch-flow-plaita.sh \
  --goal-file .dev/goals/01-my-goal.md \
  --provider deepseek
```

`launch-flow-plaita.sh` takes the same core flag surface as
`launch-flow.sh` (`--goal` / `--goal-file` / `--provider` / `--model` /
`--run-id` / `--hitl` / `--no-review` / `--no-commit` / `--max-steps` /
`--reviewer-provider`), runs in the background, and prints the run id and
the log path. The Flowcast flow at `.dev/flows/self-improve.flow.js`
(launched via `.dev/scripts/launch-flow.sh`) is retained as a
behaviourally equivalent **rollback path**. At a high level the loop is
the same on either engine:

```
1. Read goal from .dev/goals/ or .dev/ROADMAP.md
2. Launch recursive loop with coding tools (read_file, write_file, apply_patch, run_shell)
3. Agent reads the codebase, understands the goal, makes changes
4. Quality gates: cargo test / clippy / fmt (+ project gates from .flowcast/gates.json)
5. If all pass: commit the changes
6. If fail: resume-fix, then preserve the worktree (verdict `failed-preserved`)
7. Emit an observation to .dev/journal/ for the next run
```

> The legacy `.dev/scripts/self-improve.sh` bash wrapper is deprecated;
> the flow is the canonical, auditable, resumable path.

## Engines: thin flow, thick engine

| | plaita (recommended) | flowcast (rollback) |
|---|---|---|
| Launcher | `.dev/scripts/launch-flow-plaita.sh` | `.dev/scripts/launch-flow.sh` |
| Orchestration | `.dev/flows/self_improve_flow.py` — thin 45-node skeleton, compiled to `self-improve.plaita.json` | `.dev/flows/self-improve.flow.js` |
| Logic | `.dev/flows/self_improve_engine.py` — watchdog, gate fix-loops, review, commit/rebase, preserve | inline in the flow |

The plaita form splits **definition** from **execution**: definition and
observation belong to plaita-console, while execution stays local. Each
node in the skeleton is a thin shim that shells out to
`self_improve_engine.py step <name>`; the engine holds the cross-step
state. Changing loop *behaviour* therefore means editing only
`self_improve_engine.py` — no re-publish of the flow definition. Changing
the node *graph* means editing `self_improve_flow.py`, re-running
`build_self_improve_flow.py`, and publishing a new version. Definitions
resolve with a three-tier fallback (published console version → cached
copy in the run directory → the in-repo JSON), and both engines write the
same `.flowcast/runs/<run-id>/` artifacts with the same `state.json`
contract, so supervision and resume are identical.

Both engines enforce the same quality gates and emit the same verdicts —
`committed` / `failed-preserved` / `skip-commit` / `panic-preserved`.
(flowcast can additionally report `rolled-back` in one rare case: an
attempt error which coincides with a failed scene-preserve. The plaita
path always preserves instead, so it never emits `rolled-back`.) Reach for
flowcast if the plaita path misbehaves on a given goal.

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

Subscribe to the `AgentEvent` stream via a `ChannelSink` to monitor what the agent is doing:

```rust
use recursive::event::{AgentEvent, ChannelSink};
use std::sync::Arc;

let (sink, mut rx) = ChannelSink::new(128);

let mut runtime = AgentRuntime::builder()
    .llm(llm)
    .tools(tools)
    .event_sink(Arc::new(sink))
    .build()?;

// Spawn a task to consume events
tokio::spawn(async move {
    while let Ok(event) = rx.recv().await {
        match event {
            AgentEvent::ToolCall { name, arguments, .. } => {
                if name == "apply_patch" {
                    println!("Patching…");
                } else if name == "run_shell" {
                    println!("Shell: {}", arguments);
                }
            }
            AgentEvent::TurnFinished { reason, steps } => {
                println!("Done after {} steps: {}", steps, reason);
            }
            _ => {}
        }
    }
});

let outcome = runtime.run("implement the next goal").await?;
```
