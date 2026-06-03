# 多 Agent

Recursive 内置了基于相同正交原语构建的多 Agent 系统。

## 概念

| 概念 | 说明 |
|---|---|
| `AgentPool` | 按角色命名的 Agent 集合 |
| `SharedMemory` | 池中所有 Agent 共享的键值存储 |
| `MessageBus` | Agent 间通信的发布/订阅通道 |
| `Pipeline` | 串行运行 Agent，将输出作为下一个 Agent 的输入 |
| `Team` | 协调 Agent 将任务委派给专家 Agent |

## Agent Pool

```rust
use recursive::multi::{AgentPool, AgentRole};

let pool = AgentPool::new()
    .add(AgentRole::Orchestrator, orchestrator_agent)
    .add(AgentRole::Researcher, researcher_agent)
    .add(AgentRole::Coder, coder_agent);

pool.run("实现功能 X").await?;
```

## Pipeline（流水线）

```rust
use recursive::multi::Pipeline;

let pipeline = Pipeline::new()
    .step(research_agent)
    .step(planning_agent)
    .step(coding_agent)
    .step(review_agent);

pipeline.run("实现登录功能").await?;
```

## Team（团队编排）

```rust
use recursive::multi::Team;

let team = Team::new(orchestrator_agent)
    .specialist("researcher", researcher_agent)
    .specialist("coder", coder_agent)
    .specialist("reviewer", reviewer_agent);

team.run("构建用户管理 REST API").await?;
```
