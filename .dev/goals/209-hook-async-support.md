# Goal 209 — Hook System V2 P2-3: 异步 Hook 支持

**Roadmap**: Hook System V2 — Phase 2 类型扩展
**提案**: `.dev/proposals/hook-system-v2.md`
**依赖**: Goal 206（Settings 文件 + Matcher）

**Design principle check**:
- 修改 `src/hooks/external.rs` — async/asyncRewake/once 标志
- 修改 `src/hooks/config.rs` — 字段已在 Goal 206 Schema 中定义
- ❌ 不在 `agent.rs` 主循环添加新分支（通过 CancellationToken 通信）

## Why

当前所有 hook 都是同步阻塞的：hook 运行时 Agent 被暂停。
fake-cc 支持两种异步模式：
- `async: true` — hook 在后台运行，Agent 继续执行，不等 hook 结果
- `asyncRewake: true` — hook 在后台运行，若以 exit code 2 退出则中断 Agent 当前轮次

此外 `once: true` 允许 hook 执行一次后自动移除（适合 setup/initialization 类场景）。

## Scope

### 1. `once: true` 支持

在 `ExternalHookRunner` 内部维护一个 `HashSet<usize>` 记录"已执行一次"的 hook 索引。
每次 dispatch 前检查，若 hook 标记为 once 且已执行，则跳过。

```rust
// ExternalHookRunner 新增字段
executed_once: Arc<Mutex<HashSet<usize>>>,
```

执行后若 `config.once == true`，将 hook 索引加入 `executed_once`。

### 2. `async: true` 支持（fire-and-forget）

当 hook 标记为 `async: true` 时：
- `tokio::spawn` 在后台运行 hook
- 立即返回 `HookResult::continue_default()`（不等结果）
- 后台任务仅记录日志，不影响 Agent 流程

```rust
if command.r#async || command.async_rewake {
    tokio::spawn(async move {
        let result = run_hook_impl(...).await;
        if let Err(e) = result {
            tracing::warn!("async hook error: {e}");
        }
    });
    return HookResult::continue_default();
}
```

### 3. `asyncRewake: true` 支持

类似 async，但后台任务监听 exit code：
- exit code 2 → 通过 `CancellationToken` 请求中断当前 Agent 轮次
- 其他 exit code → 仅记录日志

```rust
if command.async_rewake {
    let cancel = self.cancel_token.clone(); // 从 ExternalHookRunner 持有
    tokio::spawn(async move {
        let output = run_process_hook(...).await;
        if let Ok(o) = output {
            if o.exit_code == Some(2) {
                tracing::warn!("asyncRewake hook triggered rewake");
                cancel.cancel();
            }
        }
    });
    return HookResult::continue_default();
}
```

`ExternalHookRunner` 新增字段：
```rust
pub cancel_token: Option<CancellationToken>,
```

在 `from_config` / `from_config_with_llm` 时注入（来自 Agent runtime）。

### 4. 进程退出码暴露

当前 `HookOutput` 只解析 JSON stdout。异步 hook 的 `asyncRewake` 需要监听原始 exit code，
因此 `run_hook` 内部需要同时返回 exit code：

```rust
struct RawHookOutput {
    json: Option<HookOutput>,
    exit_code: Option<i32>,
}
```

## Tests to add

1. `once_hook_runs_only_first_time` — dispatch 两次，hook 只执行一次
2. `async_hook_returns_continue_immediately` — async hook 不阻塞 dispatch 返回
3. `async_hook_runs_in_background` — 通过计数器验证后台实际执行
4. `async_rewake_exit2_triggers_cancel` — exit code 2 调用 cancel_token.cancel()
5. `async_rewake_exit0_no_cancel` — exit code 0 不触发 cancel

## Acceptance

- `cargo test --workspace` 绿色
- `cargo clippy` 干净
- `async: true` hook 不阻塞 Agent 执行
- `asyncRewake: true` hook 在 exit 2 时正确中断 Agent 轮次
- `once: true` hook 只执行一次
