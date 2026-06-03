# Multi-Agent

Recursive includes a multi-agent system built on the same orthogonal primitives.

## Concepts

| Concept | Description |
|---|---|
| `AgentPool` | A named collection of agents that can be addressed by role |
| `SharedMemory` | A key-value store shared across all agents in a pool |
| `MessageBus` | Publish/subscribe channel for inter-agent communication |
| `Pipeline` | Run agents in sequence, piping outputs as inputs |
| `Team` | An orchestrator agent that delegates to specialist agents |

## Agent Pool

```rust
use recursive::multi::{AgentPool, AgentRole};

let pool = AgentPool::new()
    .add(AgentRole::Orchestrator, orchestrator_agent)
    .add(AgentRole::Researcher, researcher_agent)
    .add(AgentRole::Coder, coder_agent);

pool.run("implement feature X").await?;
```

## Shared Memory

```rust
use recursive::multi::SharedMemory;

let memory = SharedMemory::new();

// Agents can read and write shared state
memory.set("current_file", "src/main.rs").await;
let val = memory.get("current_file").await;
```

## Message Bus

```rust
use recursive::multi::MessageBus;

let bus = MessageBus::new();

// Subscribe
let mut rx = bus.subscribe("results");

// Publish
bus.publish("results", "Agent A finished: found 3 bugs").await;

// Receive
while let Some(msg) = rx.recv().await {
    println!("Received: {msg}");
}
```

## Pipeline

```rust
use recursive::multi::Pipeline;

let pipeline = Pipeline::new()
    .step(research_agent)
    .step(planning_agent)
    .step(coding_agent)
    .step(review_agent);

pipeline.run("implement the login feature").await?;
```

## Team (Orchestrator pattern)

```rust
use recursive::multi::Team;

let team = Team::new(orchestrator_agent)
    .specialist("researcher", researcher_agent)
    .specialist("coder", coder_agent)
    .specialist("reviewer", reviewer_agent);

team.run("build a REST API for user management").await?;
```
