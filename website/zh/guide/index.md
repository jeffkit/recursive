# 介绍

**Recursive** 是一个极简、正交、可嵌入的 Rust 编码 Agent 内核。

它将以下组件串联在一起：

- **LLM Provider**（默认为 OpenAI 兼容 HTTP——支持 OpenAI、GLM/智谱、DeepSeek、Moonshot、MiniMax、Together、Ollama、vLLM 等）
- **工具注册表**（内置 `read_file`、`write_file`、`apply_patch`、`list_dir`、`run_shell`，可轻松扩展）
- **对话记录**（transcript）以及可订阅的 `StepEvent` 流

整个内核设计得足够精简，一次就能读完。

## 为什么选择 Recursive？

大多数 Agent 框架都在膨胀成"框架"——有主观的 Pipeline、LangChain 式链路、强制 UI。Recursive 始终是一个*内核*：五个正交概念，每个都可独立测试、独立替换。

| 你的需求 | Recursive 的处理方式 |
|---|---|
| 新工具 | 实现 `Tool`，注册即用。无需修改 Agent。 |
| 新模型后端 | 实现 `LlmProvider`。无需修改工具/Agent。 |
| 新 UI 或日志 | 订阅 `StepEvent` 通道。无需修改循环。 |
| 自定义终止条件 | 添加 `FinishReason` 变体。 |

## 功能概览

- **CLI**：`recursive run`、`repl`、`loop`、`http`、`tools`、`sessions`
- **HTTP API**：基于 axum 的 REST 服务，支持会话和 SSE 流式输出
- **终端 UI**：基于 ratatui 的 TUI，支持流式工具指示和计划模式
- **多 Agent**：Agent 池、共享内存、消息总线、流水线与团队编排
- **Python SDK**：`pip install recursive-client`
- **TypeScript SDK**：`npm install recursive-client`
- **Loop 模式**：自调度自主 Agent 运行

## 快速导航

- [快速开始](./quickstart) — 5 分钟内安装并运行你的第一个 Agent
- [核心概念](./concepts) — 理解五个基础构件
- [配置参考](./config) — 所有环境变量和选项
