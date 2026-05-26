# Goal 84 — Planning Mode: CLI `--plan-first` Flag

**Roadmap**: Phase 8.4 part 3/3 — CLI integration

**Design principle check**:
- Implemented as: new CLI flag in `src/main.rs` + stdin confirmation loop.
  Minimal.

## Why

The planning types (g82) and loop logic (g83) are done. This goal wires
them into the CLI so users can run `recursive run --plan-first "goal"`.

## Scope (do exactly this, no more)

### 1. `src/main.rs` — add `--plan-first` flag

Add to the Run subcommand arguments:

```rust
/// Present a plan for confirmation before executing tools
#[arg(long)]
plan_first: bool,
```

### 2. Wire it into agent building

```rust
let mut builder = Agent::builder()
    .llm(llm)
    .tools(tools)
    // ...existing config...
    ;

if args.plan_first {
    builder = builder.planning_mode(PlanningMode::PlanFirst);
}
```

### 3. Handle PlanPending finish reason

After `agent.run()` returns:

```rust
loop {
    let outcome = agent.run(&goal).await?;
    match outcome.finish {
        FinishReason::PlanPending => {
            // Print the plan
            eprintln!("\n=== Plan ===");
            // Get the plan from the last event or format from transcript
            eprintln!("Confirm? [y/n]: ");

            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            let input = input.trim().to_lowercase();

            if input == "y" || input == "yes" {
                agent.confirm_plan();
                continue;  // Re-run the loop
            } else {
                agent.reject_plan(&format!("User rejected: {}", input));
                eprintln!("Plan rejected.");
                break;
            }
        }
        _ => break,  // Normal termination
    }
}
```

### 4. Tests

- Test: `--plan-first` flag is recognized by CLI parser
- Test: without `--plan-first`, agent uses Immediate mode (regression)

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets -- -D warnings` clean
- `recursive run --plan-first "list files"` shows a plan and waits for input
- Without `--plan-first`, behavior is unchanged

## Notes for the agent

- Read `src/main.rs` for how other flags (--stream, --hook-timing, etc.)
  are wired into the agent builder.
- The confirmation loop is simple: print plan, read y/n, confirm or reject.
- Use `eprintln!` for the plan display (not println — stdout might be
  used for JSON output mode).
- This goal should be ~30-50 LOC. If you're writing more, something is wrong.
- Make sure `PlanningMode` is imported from the crate.
