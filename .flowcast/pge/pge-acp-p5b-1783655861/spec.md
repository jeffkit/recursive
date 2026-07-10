# Recursive ACP Protocol v1 — Sprint 2–4 产品 Spec

在已完成的基础层（P0 类型层、P1 stdio loop、P2 session/text prompt、P3 tool_call 通知、P4 部分 cancel/abort）之上，实现 ACP 协议的完整能力：session 历史回放与恢复、permission 交互桥、editor→agent 反向 fs、MCP 多 transport 支持（stdio/http/SSE）、session 生命周期严格清理、CLI 集成、E2E 验证。最终使任何 ACP client（Zed/JetBrains 等）能通过 `recursive acp` 将 Recursive 作为完整 coding agent 调用，涵盖 prompt→tool→cancel→load→resume 全流程。

## Sprints
### 1. Sprint 1 — Session 历史回放 + 恢复 + Permission 桥
- 作为 ACP client，调用 session/load 时能收到完整的历史消息回放（包括 user_message_chunk、agent_message_chunk、tool_call/tool_call_update），回放后 return result=null
- 作为 ACP client，调用 session/resume 时能恢复上下文但不回放历史消息，且能继续对话
- 作为 ACP client，load/resume 时传入的 mcpServers 配置被正确解析：kill 上次的 stdio 子进程 → 启动新 server → 返回 result
- 作为 ACP client，当 agent 需要 host 授权时（如文件写权限），收到 session/request_permission 通知，回复 PermissionOutcome 后 agent 继续或中止
- 作为开发者，历史回放的 messageId 基于内容 hash 生成且稳定可预测，不修改 transcript schema
- 作为 ACP client，load 时声明的能力（SessionCapabilities.resume/loadSession）在 initialize 阶段由 agentCapabilities 正确反映
- 作为开发者，permission 桥通过 PermissionHook trait 扩展而非侵入 AgentRuntime 核心逻辑

起点：P4 剩余（permission bridge）与 P5（session/load + resume）合并，因为 load/resume 的 MCP 重连逻辑依赖 session/close 的 kill-gracefully 机制，而 permission 桥是继 cancel 之后的第二个 client→agent 反方向交互模式，两者都依赖同一个 AgentRuntime→client 消息通道的扩展。本 sprint 新增两个独立交互模式。

### 2. Sprint 2 — Editor 反向 fs + MCP 多 transport + session 生命周期清理
- 作为 ACP client，声明 fs.readTextFile=true 后，agent 的 Read 工具优先请求 client 端的未保存 buffer 内容（通过 ClientReadFile 工具），否则降级本地 Read 并始终经过 sandbox resolve_within
- 作为 ACP client，agent 的 Write 工具在 client 声明 fs.writeTextFile=true 时通过 ClientWriteFile 写回 client buffer，沙箱逃逸检测依然触发
- 作为 ACP client，session/new 的 mcpServers 支持配置 stdio（起子进程）、http（远程）、SSE（远程）三种 transport，MCP bridge 根据 server.transport 分路建立连接
- 作为 ACP client，mcpServers 中与全局 MCP 配置命名冲突时，session-scoped 注册表优先于全局 config
- 作为 ACP client，session/close 时所有此 session 启动的 stdio MCP 子进程被 kill（SIGTERM + 超时 SIGKILL），无僵尸进程残留
- 作为 ACP client，session/load 和 session/resume 触发老 MCP server kill 后启动新 server，且与 Sprint 1 的 MCP 重连逻辑同一套 kill-gracefully 机制
- 作为开发者，kill 子进程的 graceful-timeout-kill 三阶段逻辑被封装为可测试的独立函数，不会因忘记 kill 而泄露

Sprint 1 完成后 permission 通道已通，本 sprint 在已有通道上叠加两个垂直能力：editor→agent 反向文件系统（客户声明 fs.readTextFile=true 时优先用 client 未保存 buffer）和 MCP SDK 的三种传输协议（stdio/http/SSE）全覆盖。session/close 必须 kill stdio 子进程以防泄露。沙箱校验在两种路径上都跑。

### 3. Sprint 3 — CLI 子命令 + E2E + 不变式测试 + 产品化收尾
- 作为终端用户，运行 `recursive acp --llm-provider openai` 即可通过 stdio 与任意 ACP client 对话，子命令与 existing `recursive mcp`/`recursive http` 并列在 help 中
- 作为 E2E 测试工程师，运行 E2E 测试（scripted ACP client）覆盖 prompt→tool→cancel→load 完整流程，assert notification 序列（user_message_chunk → agent_message_chunk → tool_call → tool_call_update → end_turn → 回放完整历史）
- 作为 CI/CD 平台，不变式测试自动检测：ACP 代码未在 run_inner 中加分支（AST 检查）、ACP host fs 走 resolve_within（沙箱逃逸检测）、cancel 后 tool-call↔tool-result 配对仍成立
- 作为稳定性工程师，所有新增代码无 unwrap()/expect()（非 test），cargo test/clippy/fmt 全绿，且通过最终手动验收测试：从 Zed 配置 `recursive acp` 为编码 agent 并完成一次完整对话
- 作为 Zed 用户，配置 `.zed/settings.json` 的 `agent` 指向 `recursive acp` 后，编辑器内直接召唤 Recursive 作为 coding agent，支持编辑对话、工具调用、取消、历史加载

前两个 sprint 完成全部协议能力后，本 sprint 做产品化包装。`recursive acp` CLI 子命令与 `mcp`/`http` 并列，提供 `--llm-provider` 等参数。E2E 用 scripted ACP client 覆盖 prompt→tool→cancel→load 全流程。新增 3 个不变式测试（run_inner 分支检查、resolve_within 沙箱逃逸检测、cancel 后 tool-call↔tool-result 配对）。最终用 Zed 手动验收。
