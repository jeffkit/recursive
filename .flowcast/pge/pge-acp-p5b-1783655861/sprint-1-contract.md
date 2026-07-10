# Contract: Sprint 1 — Session 历史回放 + 恢复 + Permission 桥

实现 ACP 协议的两个新增独立交互模式：session/load 历史回放与 session/resume 上下文恢复，以及 permission 桥。两者共享 AgentRuntime→client 消息通道的扩展。

## Criteria
- [S1.1] ACP client 调用 session/load 时收到完整历史消息回放，包含 user_message_chunk、agent_message_chunk、tool_call、tool_call_update 四种 notification 类型，回放完毕后返回 result=null
  - how: 启动一个已有历史消息的 session，scripted ACP client 发送 session/load 请求，录得 notification 序列。验证：
1. 四种 chunk 类型均出现且按原始时间序排列
2. 每个 chunk 的 content 与原始 transcript 一致
3. 最后一个消息后的 response 的 result 字段为 null（或空 JSON object）
- [S1.2] ACP client 调用 session/resume 时不收到历史消息回放，但 restore 上下文后能继续对话，新 prompt 被正常处理并产生 tool_call/end_turn
  - how: 对已结束的 session 调用 session/resume，验证：
1. resume 响应中不含任何 user_message_chunk / agent_message_chunk / tool_call / tool_call_update notification
2. resume 后立即发送新一轮 text prompt
3. 新 prompt 的 response 包含完整的工具调用流（tool_call → tool_call_update → end_turn），说明上下文已恢复
- [S1.3] session/load 和 session/resume 传入的 mcpServers 被正确解析：kill 上次 stdio 子进程 → 启动新 server → 返回 result，无僵尸进程残留
  - how: 1. 启动 session（配置 mcpServers 含一个 stdio MCP server，如 count-lines MCP 服务）
2. 记录初始 server 子进程 PID
3. 调用 session/load 或 session/resume，传入相同或不同的 mcpServers 配置
4. 验证：
   a) 旧 PID 的进程已消失（ps 确认）
   b) 新 stdio MCP server 子进程已启动且可正常工具调用
   c) load/resume 的 response 包含 result
   d) session/close 后该新子进程也被 kill，无残留
- [S1.4] 当 agent 需要 host 授权（如文件写权限）时，ACP client 收到 session/request_permission 通知，回复 PermissionOutcome 后 agent 根据 outcome 继续或中止
  - how: 1. 发送触发写文件的 prompt（如「写入文件 /tmp/test.txt」）
2. 验证 agent 没有直接执行 Write 工具，而是发出 session/request_permission notification
3. notification payload 含 permission_id、tool_name、参数详情
4. 发送 PermissionOutcome（granted=true）→ agent 继续执行 Write 工具调用
5. 另开测试：PermissionOutcome（granted=false）→ agent 发出 tool_call 但跳过执行或回复「已取消」
6. 验证无超时或崩溃
- [S1.5] 历史回放的 messageId 基于内容 hash 生成，相同内容在不同 session/load 调用中得到相同的 messageId，且 transcript schema 未新增字段
  - how: 1. 创建 session，写入已知内容的 user message 和 agent message
2. 先后两次调用 session/load
3. 两次回放中对应消息的 messageId 字段相同
4. 导出 transcript JSON，对比原始 schema（通过 git diff 确认 transcript.jsonl 的每行字段数/名称未变）
5. 唯一新增可能是 messageId 字段本身，确认它只出现在 ACP 通信层，不写入 transcript 持久化结构
- [S1.6] session/load 时声明的 SessionCapabilities（resume、loadSession）在 initialize 阶段的 agentCapabilities 中正确反映
  - how: 1. scripted ACP client 发送 initialize 请求，其中 capabilities 不声明 resume/loadSession
2. agentCapabilities 中 resume 和 loadSession 应均为 false
3. 新开连接，client 的 capabilities 声明 resume=true、loadSession=true
4. agentCapabilities 中对应字段返回 true
5. 验证 client 不声明时、部分声明时、全部声明时 agentCapabilities 都正确映射
- [S1.7] Permission bridge 通过 PermissionHook trait 扩展实现，不侵入 AgentRuntime 核心逻辑
  - how: 1. 确认项目中存在 PermissionHook trait 定义（trait 含 request_permission 等方法）
2. 确认 AgentRuntime 通过 Box<dyn PermissionHook> 成员引用该 trait，而非在 run_inner 中硬编码 permission 逻辑分支
3. 确认默认实现（PermissionHookDisabled）存在，且 permission 未配置时 behave 为跳过授权直接执行
4. 确认无 unwrap/expect 出现在 permission hook 相关代码中（除 test 外）
5. 通过 grep 确认 run_inner 方法没有新增 if/else 分支处理 permission（invariant #1）