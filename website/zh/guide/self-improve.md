# 自我改进 Agent

Recursive 最独特的特性之一是它运行着自己的开发循环。你用于构建自己工具的同一个 Agent 内核，也被用来在 Recursive 本身中实现新功能。

## 工作原理

自我改进循环由 Flowcast flow（`.dev/flows/self-improve.flow.js`，经
`.dev/scripts/launch-flow.sh` 启动；详见 `.dev/flows/SELF_IMPROVE.md`）
编排。核心流程如下：

```
1. 从 .dev/goals/ 或 .dev/ROADMAP.md 读取目标
2. 启动 recursive loop，配备编码工具（read_file、write_file、apply_patch、run_shell）
3. Agent 读取代码库，理解目标，进行修改
4. 质量门：cargo test / clippy / fmt（+ .flowcast/gates.json 里的项目门）
5. 全部通过：提交变更
6. 失败：resume-fix 一次，仍失败则回滚
7. 向 .dev/journal/ 写入观察记录，供下次运行参考
```

> 旧的 `.dev/scripts/self-improve.sh` bash 包装器已弃用；flow 才是
> 可审计、可观测、可断点续跑的 canonical 路径。

## 观察系统

每次运行后，Agent 在 `.dev/journal/` 中写入日志条目，包含：
- 尝试了什么
- 成功或失败的原因
- 下次的经验教训

*下次*运行时，Agent 会在开始之前读取近期日志。这形成了持久的反馈循环——Agent 从错误中学习，无需任何外部训练。

## 核心不变量

自改进循环强制执行记录在 `.dev/AGENTS.md` 中的几条不变量：

| 不变量 | 说明 |
|---|---|
| #1 | Agent 循环保持精简——新能力以工具形式添加，不改变循环 |
| #3 | 沙箱——所有 fs/shell 工具使用 `resolve_within` |
| #5 | 产品代码中不允许 `unwrap()` |
| #8 | 工具调用 ↔ 工具结果配对必须保留 |

这些不变量*通过代码检查*——clippy 和测试强制执行，不仅仅是文档约定。

## 在你自己的项目中使用这个循环

同样的模式可以应用于任何代码库：

1. 创建 `.dev/goals/` 目录，放置目标文件
2. 在项目根目录添加 `AGENTS.md`，描述不变量、约定和上下文
3. 运行 `recursive loop --workspace . "读取 .dev/goals/ 并实现下一个未完成的目标"`

```bash
# 创建目标
cat > .dev/goals/01-add-caching.md << 'EOF'
## 目标：为 API 层添加内存缓存

/api/users 接口因为每次请求都查询数据库而很慢。
使用 RwLock 包装的 HashMap 添加简单的 TTL 缓存（5 分钟）。

验收标准：
- 负载测试中缓存命中率 > 80%
- 无数据竞争（使用 Arc<RwLock<...>>）
- 写操作时使缓存失效
EOF

# 运行循环
recursive loop "读取 .dev/goals/ 并实现下一个未完成的目标"
```

## apply_patch 使用纪律

观察系统追踪的一个指标是 **`apply_patch` : `write_file` 比率**：

- 高比率 = Agent 进行外科式修改 → 好
- 低比率 = Agent 频繁在 `apply_patch` 失败后退回到重写整个文件 → 说明补丁锚点不精确

当 `apply_patch` 因上下文行歧义而失败时，正确的处理是扩大锚点范围，而不是退回到 `write_file`。

## 监控运行过程

通过 `ChannelSink` 订阅 `AgentEvent` 流，实时监控 Agent 的行为：

```rust
use recursive::event::{AgentEvent, ChannelSink};
use std::sync::Arc;

let (sink, mut rx) = ChannelSink::new(128);

let mut runtime = AgentRuntime::builder()
    .llm(llm)
    .tools(tools)
    .event_sink(Arc::new(sink))
    .build()?;

// 启动任务消费事件
tokio::spawn(async move {
    while let Ok(event) = rx.recv().await {
        match event {
            AgentEvent::ToolCall { name, arguments, .. } => {
                if name == "apply_patch" {
                    println!("正在打补丁…");
                } else if name == "run_shell" {
                    println!("执行命令：{}", arguments);
                }
            }
            AgentEvent::TurnFinished { reason, steps } => {
                println!("完成，共 {} 步，原因：{}", steps, reason);
            }
            _ => {}
        }
    }
});

let outcome = runtime.run("实现下一个目标").await?;
```
