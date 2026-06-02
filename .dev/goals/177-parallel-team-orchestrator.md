# Goal 177 — Parallel TeamOrchestrator: 并行执行委派任务

**Roadmap**: Phase 18 — Advanced Agent Patterns (multi-agent improvements)
**Design principle check**:
- 仅改 `src/multi.rs` — TeamOrchestrator 实现
- ❌ 不在 `agent.rs::Agent::run` 主循环里加分支
- ✅ 纯内部改造：保持公开 API 不变，只改执行模型

## Why

当前 `TeamOrchestrator::run` 在执行委派任务时是**顺序**的：

```
lead 规划 → delegate to role_a (等待) → delegate to role_b (等待) → 综合
```

对于相互独立的委派任务（如「coder 写代码」+ 「researcher 查资料」），
顺序执行浪费时间。改成并行执行后，N 个委派任务同时运行，总耗时降至最慢任务的耗时。

## What this goal does

### 修改 `src/multi.rs` — TeamOrchestrator::run

将委派执行阶段从顺序循环改为 `tokio::join_all` 并行执行：

**Before (sequential)**:
```rust
for (role, task) in &delegations {
    match pool.run_with_role(role, task).await {
        ...
    }
}
```

**After (parallel)**:
```rust
let futures: Vec<_> = delegations.iter().map(|(role, task)| {
    pool.run_with_role(role, task)
}).collect();
let results = futures::future::join_all(futures).await;
```

### 注意事项

- `AgentPool::run_with_role` 当前接受 `&self`（不可变引用），可以安全并行调用
- 需要确认 `AgentPool` 是 `Sync` 的（内部都是 `Arc<RwLock<_>>`，应该没问题）
- 测试需要验证并行结果的顺序与 delegations 列表一致

## Files to change

| File | Change |
|------|--------|
| `src/multi.rs` | 将 `TeamOrchestrator::run` 的委派执行改为 `join_all` 并行 |

## Acceptance

1. `cargo test --workspace` 全绿
2. `cargo clippy --all-targets --all-features -- -D warnings` 干净
3. 现有 `TeamOrchestrator` 测试通过（行为不变，只是更快）
4. 新增测试验证并行执行（通过时序或 mock 验证多任务同时运行）
