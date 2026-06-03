# 更新日志

## v0.6.0（当前）

- 权限系统 v2：细粒度的工具级权限和审批流程
- 无 `unwrap` 代码库：所有产品代码使用正确的错误传播
- TUI 改进：Markdown 渲染、计划模式、滚动、会话管理
- 多 Agent 增强：流水线、团队编排、共享内存总线
- 改进的对话记录压缩（基于 LLM 的摘要）

## v0.5.0

- **HTTP API** — 基于 axum 的 REST 服务，支持会话和 SSE 流式输出
- **终端 UI** — 基于 ratatui 的 TUI，支持流式工具指示和计划模式
- **多 Agent** — Agent 池、共享内存、消息总线、流水线与团队编排
- **Python SDK** — `pip install recursive-client`
- **TypeScript SDK** — `npm install recursive-client`
- **Loop 模式** — `recursive loop` 自调度自主 Agent 运行

## v0.2.0

- Skill 系统 v2：引用、脚本、参数、注入模式、组合
- MCP HTTP+SSE 传输
- MCP 资源和提示支持
- Feature 标志：`mcp`、`web_fetch`、`anthropic`
- 结构化错误类型
- 5 个可运行示例
- 367+ 测试

## v0.1.0

- 极简 ReAct Agent 循环
- OpenAI 兼容 LLM Provider
- 文件系统工具：读、写、列出、打补丁
- 带沙箱和超时的 Shell 工具
- 用于离线测试的 Mock Provider
- CLI：`run`、`repl`、`tools` 命令
- 生命周期观察的 Hook 系统
- 对话记录压缩
- MCP stdio 传输
- Skill 系统 v1
