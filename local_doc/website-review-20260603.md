# Recursive 文档站 Review

**日期**：2026-06-03  
**审阅者**：小美（以开发者 + 用户双重身份）  
**VitePress 骨架状态**：已创建，en/zh 双语结构完整，但所有文件仍为 untracked（未提交）

---

## 总体印象

文档站结构清晰，导航层次合理，中英双语支持到位。Quickstart 和 Core Concepts 的质量不错。但整体偏向"参考文档"，缺少"带我做一个真实项目"的教程风格内容。对于核心创新（自改进循环、正交设计哲学）的传达力不够强。

---

## 一、结构 / 导航问题

### 1.1 `multi-agent/` 目录是死路
- `website/en/multi-agent/index.md` 只有一行话，把用户指向 `library/multi-agent`
- `website/zh/multi-agent/index.md` 同理
- **问题**：顶部导航栏没有 "Multi-Agent" 入口，`multi-agent/` 目录事实上不可被发现
- **建议**：要么给这个目录做实质性内容（端到端 Multi-Agent 教程），要么从 VitePress 配置中去掉这个目录，把相关内容都放到 Library → Multi-Agent

### 1.2 Changelog 无侧边栏入口
- `en/changelog.md` 在顶部导航可见，但没有出现在任何侧边栏，用户读着读着找不到它
- **建议**：在 Guide 侧边栏底部加一个 Changelog 链接，或者在 footer 加链接

### 1.3 zh 侧边栏缺 multi-agent 下的 library 入口
- 中文 `zh/library/` 侧边栏配置中包含了 `multi-agent`（`/zh/library/multi-agent`），但该文件存在吗？需要核实
- 整个 zh 的 library 页面质量参差不齐

---

## 二、内容缺失

### 2.1 最核心的创新——自改进循环——几乎没有被记录
Recursive 最大的特色之一是它能驱动自己的开发（`.dev/scripts/self-improve.sh`、AGENTS.md 体系）。但文档站完全没有提及：
- 什么是 self-improve loop？
- 如何让 Recursive 改进自己（或你的项目）？
- 观测系统（StepEvent、patch/write 比率）是怎么工作的？

**建议**：在 Guide 下增加一篇 "Self-Improving Agents" 或 "Loop Mode Deep Dive"。

### 2.2 `agui-*` crates 无文档
项目包含 `crates/agui-protocol`、`crates/agui-client`、`crates/agui-tui` 三个 crate，这是 AG-UI 协议集成层，但文档站完全没有提及：
- 这三个 crate 是什么？
- 什么场景下用？
- 和主 `recursive-agent` 的关系是什么？

### 2.3 缺乏真实世界示例（Recipe / Cookbook）
目前 Quickstart 只到"运行一个 hello world 级别"的任务。没有：
- 如何构建一个代码审查 Agent？
- 如何构建一个文件组织工具？
- 如何用 Pipeline + Team 做多步自动化？

**建议**：在 Guide 下增加 "Examples" 或 "Recipes" 子页。

### 2.4 `apply_patch` 工具没有文档
这是 Recursive 自改进的核心工具（V4A patch format），但 `en/library/tools.md` 里没有专门介绍。README 里有提但文档站没有。

### 2.5 providers.toml 配置文件没有实际示例
Config 页虽然提了 `providers.toml`，但例子太简单，没有说明：
- 如何切换 provider 做 fallback？
- 多 provider 并行时怎么配？

### 2.6 缺乏 FAQ / 故障排查页
常见问题：
- API Key 配置错误怎么排查？
- 沙箱权限拒绝了怎么办？
- 自改进循环卡住了怎么处理？

---

## 三、开发者体验问题

### 3.1 没有"接入已有 Rust 项目"的向导
用户最可能的场景不是"从零新建"而是"把 Recursive 嵌入我已有的 axum 应用 / Tauri 应用"。缺乏这类指导。

### 3.2 Library API 示例代码无法验证是否是最新 API
- `multi-agent.md` 里用 `AgentPool::new().add(AgentRole::Orchestrator, ...)` 和 `Pipeline::new().step(...)` — 这些 API 名称是否和实际源码一致？需要核实
- `FinishReason::Error(e)` — 实际源码的枚举 variant 名称是否如此？

### 3.3 Python/TypeScript SDK 发布状态不明
文档说 `pip install recursive-client` 和 `npm install recursive-client`，但没有说明：
- 这两个包是否实际已发布到 PyPI / npm？
- 当前版本号？
- 如果还没发布，应给出"本地安装"的方式

### 3.4 Docker 镜像地址未验证
`docker pull ghcr.io/jeffkit/recursive:latest` — 是否已推送？建议加上"如未推送请用 `cargo install`"的备注。

---

## 四、用户体验（UX）问题

### 4.1 首页没有截图 / Demo
`website/index.md` 的 hero section 没有任何截图、GIF 或 Asciinema 录像。
- 用户无法在 5 秒内感受到"这个工具实际跑起来是什么样子"
- **建议**：在 hero 下方加一个 TUI 截图或终端录像，展示 `recursive repl` 的运行效果

### 4.2 Feature cards 过于抽象
首页 Features 写的是"Rust-native"、"Truly orthogonal"、"Embeddable library" —— 这些是实现语言和设计原则，不是用户关心的"我能用它做什么"。
- **建议**：改为"用 3 行代码接入任意 LLM"、"10 分钟构建代码审查 Agent"之类的行动导向描述

### 4.3 Core Concepts 图示可以更丰富
`guide/concepts.md` 里的 ASCII 架构图是个好主意，但可以考虑做成 SVG/图片版本，在视觉上更清晰。

### 4.4 没有版本号和更新日期
首页或文档头部没有显示当前对应的 Recursive 版本（v0.6.0），用户不知道文档是否和自己安装的版本匹配。

---

## 五、技术准确性

### 5.1 zh index.md 缺 TypeScript SDK 提及
`website/zh/guide/index.md` 的"功能概览"里没有列出 TypeScript SDK，但英文版有。

### 5.2 `RECURSIVE_PROVIDER_TYPE=anthropic` 示例
Quickstart 里 Anthropic 配置示例用的是 `claude-sonnet-4-5`，但 Anthropic 的最新模型命名是 `claude-sonnet-4-5` 还是其他？建议用更稳定的模型名或加注释说明需替换。

### 5.3 Library multi-agent.md 结尾被截断
读到 Team 示例代码时内容不完整（`Team::new(...)` 示例没有结束）。需要补全。

---

## 六、优先级建议

| 优先级 | 问题 | 工作量 |
|--------|------|--------|
| P0 | 首页加截图/GIF | 小 |
| P0 | 确认 SDK 发布状态并在文档中说明 | 小 |
| P0 | 修复 library/multi-agent.md 截断问题 | 小 |
| P1 | 增加至少一个真实场景 Recipe | 中 |
| P1 | 增加 Self-Improving Loop 介绍页 | 中 |
| P1 | multi-agent/ 目录要么做实要么删掉 | 小 |
| P2 | agui-* crates 文档 | 大 |
| P2 | FAQ / 故障排查 | 中 |
| P2 | 首页 Feature cards 改为行动导向描述 | 小 |
| P3 | apply_patch 工具文档 | 小 |

---

*本文档由 AI 生成，供作者参考，欢迎补充修正。*
