# Contract: Sprint 2 — P2: session/new + session/prompt (text-only) (rev 2)

首条端到端链路：session/new 创建沙箱 session，session/prompt 发 text 消息并收到流式 agent_message_chunk + end_turn，EventSink 映射到 ACP notification。暂不做 tool_call 通知、permission、fs。

## Criteria
- [C1] session/new 创建 session 成功，返回合法 sessionId
  - how: 发送 `session/new` 请求（指定 cwd），断言：① response.result 含 `sessionId` 字段且为非空字符串；② 同一 client 两次连续 `session/new` 得到的 sessionId 不同（唯一性）；③ sessionId 在后续 session/prompt 中可复用（稳定性）。
- [C2] session/new 缺 cwd 时拒绝并返回明确错误
  - how: 发送 `session/new` 不含 `cwd`，断言：① response 为 JSON-RPC error；② error.code 非零；③ error.message 明确提示 cwd 为必填。
- [C3] session/new 指定的 cwd 不存在或不可读时拒绝
  - how: 发送 `session/new` 指定不存在的路径作为 cwd，断言：① response 为 JSON-RPC error；② error.message 含路径相关信息。
- [C4] session/prompt 发送 text 消息后开始接收流式 agent_message_chunk 通知
  - how: 先创建 session，再发送 `session/prompt`（text: 'echo hello'），断言：① 在收到 `end_turn` 之前，至少收到 1 条 `notifications/agent_message_chunk`；② 每条 chunk 的 `sessionId` 与创建 session 时一致；③ chunk 的 `content` 为非空字符串。
- [C5] session/prompt 最终返回 end_turn 通知，包含 stopReason
  - how: 同一轮 prompt 完成后，断言：① 最后一条 notification 为 `notifications/end_turn`；② `end_turn` 的 `sessionId` 一致；③ `stopReason` 为有效值（正常完成时为 'end_turn' 或等效正常终止原因）。
- [C6] session/prompt 的 response.id 与请求 id 匹配
  - how: 发送 `session/prompt` 请求（含 `id`），断言：① 若不报错，最终返回的 JSON-RPC response（或最终 result notification）的 `id` 与请求 `id` 一致。
- [C7] session/prompt sessionId 无效时返回错误
  - how: 不创建 session，直接发送 `session/prompt` 伪造 sessionId，断言：① response 为 JSON-RPC error；② error.message 含 'session' 或 'not found' 类提示。
- [C8] 同一消息的所有 agent_message_chunk 共享一致的 messageId
  - how: 发送 session/prompt，抓取该轮产生的全部 `notifications/agent_message_chunk`，断言：所有 chunk 的 `messageId` 相同（同一消息的分片归属同一 messageId），且 messageId 为非空字符串。
- [C9] agent_message_chunk 通知格式符合 ACP v1 规范
  - how: 抓取一条 `notifications/agent_message_chunk`，断言：① 包含 `sessionId`、`messageId`、`content` 三个必填字段；② `messageId` 格式为非空字符串；③ `content` 格式与非流式 agent_message 的 content 一致（即 ACP 规范定义的 TextPart 或 ContentBlock 结构）。
- [C10] end_turn 通知格式符合 ACP v1 规范
  - how: 抓取 `notifications/end_turn`，断言：① 包含 `sessionId`、`stopReason` 字段；② `stopReason` 为 ACP 定义的合法值之一。
- [C11] session/prompt 的 cwd 自动作为 resolve_within 沙箱根，保证安全隔离
  - how: ① 在 `/tmp/sandbox-test/a.txt` 写入 'hello'（该路径在 session cwd 内），在 `/tmp/outside.txt` 写入 'secret'（该路径在 sandbox 外）；② 创建 session 时指定 cwd 为 `/tmp/sandbox-test`；③ 发送 session/prompt 要求 agent 读取 `../outside.txt` 或 `/tmp/outside.txt`；④ 断言 agent 的响应表明无法访问沙箱外部路径（拒绝访问或文件不存在错误），且不能泄露出 `/tmp/outside.txt` 的内容。
- [C12] session/new 重复指定同一 cwd 时 session 互相隔离不污染
  - how: ① 在 cwd 内创建文件 `a.txt`（内容 'first'）；② 创建 session1；③ 关闭 session1 后在同 cwd 创建 session2；④ session2 的 prompt 应能正常访问 `a.txt`；⑤ 两个 session 的 sessionId 不同。
- [C13] 并发多个 session 互不影响
  - how: ① 创建两个 session（不同 cwd）；② 分别向二者发送 session/prompt；③ 断言两路 stream 的 sessionId 分别正确、不交叉污染、各自收到 end_turn。
- [C14] initialize 返回的 agentCapabilities 声明 session/new 和 session/prompt 为 true
  - how: 执行 initialize 握手，断言：① `result.agentCapabilities.promptCapabilities.text` 为 `true`；② `result.agentCapabilities.sessionCapabilities` 声明支持 `create`。
- [C15] Agent 的 LLM 实际被调用并产生有意义的回复（非 mock 空回）
  - how: 发送 session/prompt 文本 'echo hello'，断言：① 流式 agent_message_chunk 的累积内容中包含 'hello' 或语义相关的合理回复；② 总 chunk 数量 > 0。
- [C16] session/prompt 缺 text 字段、text 为空、或 messages 为空时返回错误
  - how: 分别发送三个 session/prompt 请求：① 不含 `text` 字段；② `text` 为空字符串 ''；③ `messages` 为空数组 []。断言每个均为 JSON-RPC error，且 error.message 提示 text 或 messages 为必填或无效。