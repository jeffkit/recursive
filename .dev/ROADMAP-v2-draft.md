# ROADMAP v2 — From Kernel to Platform (DRAFT)

> **Status**: 草案，待用户确认方向后正式替代原始 ROADMAP 的 Priority Matrix。
> **上下文**: 原始 ROADMAP (Phase 0-4 + D-series) 100% 完成。Recursive 已从
> "interesting demo" 成长为一个功能完备的 agent kernel：支持 MCP、Skills、
> Streaming、Compaction、Hooks、Transport 抽象、Sub-agent、Memory、Session
> 管理。接下来要做什么？

## 方向选择

三条路，不互斥但需要定优先级：

### 路线 A：打磨发布（Make it Shippable）

目标：让 recursive-agent 成为一个**别人能用**的 crate + CLI。

| ID | Feature | Effort | 说明 |
|----|---------|--------|------|
| A.1 | API 稳定化 + Breaking Change 清理 | M | 审视公开 API，去掉实验性字段，加 `#[doc(hidden)]` |
| A.2 | 完善 README / docs.rs | S | 从 AGENTS.md 的内部视角切换为用户视角文档 |
| A.3 | examples/ 目录 | S | basic.rs, with_hooks.rs, with_mcp.rs, sub_agent.rs |
| A.4 | crates.io 发布 + CI release workflow | S | 版本号 0.2.0，changelog |
| A.5 | Error 类型精化 | S | 目前 anyhow 为主，library 消费者需要结构化错误 |
| A.6 | Feature flags | M | 让 MCP/web_fetch/anthropic 可选编译 |

### 路线 B：能力扩展（Make it Smarter）

目标：让 agent 能做**更复杂的任务**。

| ID | Feature | Effort | 说明 |
|----|---------|--------|------|
| B.1 | Tool Transport: SSH adapter | M | 真正的远程执行能力 |
| B.2 | Tool Transport: Docker adapter | M | 容器化沙箱 |
| B.3 | Multi-turn Planning | L | Agent 先输出计划，确认后执行（plan → execute 分离）|
| B.4 | Parallel Tool Execution | M | 多个独立 tool call 并发执行 |
| B.5 | Context Window Awareness | S | 动态调整策略基于模型 context 大小 |
| B.6 | Diff-Aware Apply Patch | M | 支持 git diff 格式，不只是 V4A |
| B.7 | File Watcher / Event-Driven | L | 监听文件变化自动触发 agent 反应 |

### 路线 C：生态接入（Make it Connected）

目标：让 Recursive 能嵌入到**更多场景**中。

| ID | Feature | Effort | 说明 |
|----|---------|--------|------|
| C.1 | MCP Server Mode | L | Recursive 自身作为 MCP server 暴露能力 |
| C.2 | HTTP API / REST Wrapper | M | 让 Recursive 可以被 web 应用调用 |
| C.3 | Language Server Protocol (LSP) | L | IDE 集成的基础 |
| C.4 | Webhook / Event Bridge | M | 接收外部事件触发 agent 运行 |
| C.5 | Multi-Agent Orchestration | L | 多个 Recursive 实例协作 |

---

## 我的建议排序

考虑到项目定位（"minimal embeddable kernel"）和你开源的意图：

**优先级 1 — 路线 A（打磨发布）**
- 最高杠杆：让已有的 15+ 特性对外部用户可用
- 低风险：不加新功能，只打磨已有的
- 预计 2 个 batch（8 个 S-effort goals）

**优先级 2 — 路线 B 选取（能力关键项）**
- B.1 SSH Transport + B.4 Parallel Tool 是最实用的
- B.3 Multi-turn Planning 是差异化特性
- 预计 2-3 个 batch

**优先级 3 — 路线 C 选取（生态）**
- C.1 MCP Server Mode 是最自然的延伸（已有 MCP Client）
- C.2 HTTP API 让非 Rust 用户也能嵌入
- 预计 2 个 batch

**总计如果全做：6-7 个 batch（~2 周的 wall-clock time）**
**如果只做 A + B 精选：4 个 batch**

---

## 等待你的决策

1. 三条路线的优先级排序你同意吗？
2. 每条路线里有没有你特别想做或特别不想做的？
3. 有没有我漏掉的方向？（比如：性能优化？多语言绑定？WASM？）
4. 你希望 v0.2 的定位是什么？— "stable kernel for embedding" 还是 "full-featured CLI agent"？
