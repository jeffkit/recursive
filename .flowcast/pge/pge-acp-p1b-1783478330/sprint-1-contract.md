# Contract: 1 (rev 2)

地基 sprint：实现 stdio JSON-RPC 2.0 transport 层和 initialize 握手。`recursive acp` 子命令启动后从 stdin 读取 newline-delimited JSON-RPC 请求，stdout 写响应，stderr 写日志。initialize 返回完整 AgentCapabilities 结构和 agentInfo。包含完整的握手协议防护：ordering enforcement（initialize 前拒绝一切非 initialize 请求）、重复 initialize 拒绝、params 校验。

## Criteria
- [S1-C1] `recursive acp` 作为 CLI 子命令存在，与 `recursive mcp` / `recursive http` 并列。执行 `recursive --help` 可见 acp 条目。执行 `recursive acp` 启动后立即进入阻塞等待 stdin 输入的状态（无参数错误退出）。
  - how: `cargo run -- acp` 启动进程，pstree 确认进程存活且未立即退出。`cargo run -- --help | grep acp` 返回非空。
- [S1-C2] ACP server 从 stdin 逐行读取 newline-delimited JSON-RPC 2.0 请求。每行是一个完整 JSON 对象。读取到 EOF 时优雅退出（exit 0）。
  - how: `echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}' | cargo run -- acp`，进程读完后正常退出，不 panic、不悬挂等待。
- [S1-C3] ACP server 将所有 JSON-RPC 响应写入 stdout（每行一个 JSON 对象，newline 分隔）。日志（tracing / eprintln）写入 stderr。stdout 只包含合法 JSON-RPC 2.0 消息，不含任何非 JSON 文本。
  - how: `echo '...initialize...' | cargo run -- acp 2>/tmp/acp_stderr > /tmp/acp_stdout`，然后 `jq . /tmp/acp_stdout` 能成功解析每一行；`cat /tmp/acp_stderr` 只包含 tracing 日志（如 INFO 级别启动日志），不含 JSON-RPC 消息。
- [S1-C4] 收到合法的 `initialize` 请求时，返回正确的 `InitializeResponse`。response 必须包含与原请求相同的 `id` 字段，`jsonrpc` 字段为 `"2.0"`。response 的 `result` 对象中包含 `protocolVersion`（值为 1，number 类型）、`agentInfo`（含 `name` 和 `version` 两个字符串字段），以及 `agentCapabilities`。
  - how: `echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientInfo":{"name":"zed","version":"0.1.0"},"clientCapabilities":{}}}' | cargo run -- acp 2>/dev/null`，stdout 输出中 `jq '.result.protocolVersion'` 返回 `1`，`jq '.result.agentInfo.name'` 返回字符串（如 `"recursive"`），`jq '.result.agentInfo.version'` 返回字符串（如 `"0.7.0"`）。
- [S1-C5] `agentCapabilities` 字段完整声明——包含 `promptCapabilities`、`mcpCapabilities`、`sessionCapabilities`、`fsCapabilities` 等所有 capability 子对象。Sprint 1 中大部分子能力为 false/空对象，但结构完整不缺失字段。
  - how: 发送 initialize 请求后，`jq '.result.agentCapabilities'` 不返回 null，且 `jq '.result.agentCapabilities | keys'` 包含 `promptCapabilities`、`mcpCapabilities`、`sessionCapabilities`、`fsCapabilities`。
- [S1-C6] `agentCapabilities.promptCapabilities` 声明 `text: true`（作为 coding agent 的基础能力）。其他具体能力字段在后续 sprint 逐步开启。
  - how: `jq '.result.agentCapabilities.promptCapabilities.text'` 返回 `true`。
- [S1-C7] 收到格式不合法的输入（非 JSON、或 JSON 但缺少 `jsonrpc` 字段）时，返回 JSON-RPC 2.0 Parse error（code: -32700）。返回的 error response 中：若原请求有 `id`，则 `id` 字段回传；若无 `id` 或无法解析，则不返回任何响应。
  - how: `echo 'not json' | cargo run -- acp 2>/dev/null` 输出 `{"jsonrpc":"2.0","error":{"code":-32700,"message":"Parse error"},"id":null}`。再测 `echo '{"id":1}' | cargo run -- acp 2>/dev/null`，输出 error 且 `id` 为 `1`。
- [S1-C8] 在 initialize 成功完成后，收到合法 JSON-RPC 2.0 但 method 不是已知方法的请求时，返回 Method not found error（code: -32601, message 中包含 "Method not found"）。Sprint 1 只实现 initialize，其他所有 method 均报 method not found。
  - how: 先发送一个合法的 initialize 请求完成握手，再发送 `{"jsonrpc":"2.0","id":2,"method":"session/new","params":{}}`，`jq -s '.[1].error.code'` 返回 `-32601`。
- [S1-C9] 收到 JSON-RPC 2.0 Notification（合法 JSON-RPC 对象但无 `id` 字段）时，server 接受并静默处理，不往 stdout 写入任何响应。
  - how: `echo '{"jsonrpc":"2.0","method":"notifications/initialized"}' | cargo run -- acp 2>/dev/null` 输出为空（0 行）。
- [S1-C10] 所有现有质量门保持绿色：`cargo test --workspace` 全部通过、`cargo clippy --all-targets -- -D warnings` 零警告、`cargo fmt --all --check` 无 diff。新增代码的单元测试覆盖率覆盖 transport loop、initialize 响应构建、错误路径、握手状态机（ordering enforcement、重复 initialize、params 校验）。
  - how: 在 worktree 内依次执行三条命令，全部 exit 0。`cargo test --workspace` 输出中可见 `src/acp/server.rs` 相关测试 PASS。
- [S1-C11] ACP 协议要求 initialize 是客户端发的第一个请求。在 initialize 成功完成前，任何其他 method 的请求（无论 method 是否存在）都应返回 Server not initialized 错误（code: -32002, message: "Server not initialized"）。
  - how: `echo '{"jsonrpc":"2.0","id":2,"method":"session/new","params":{}}' | cargo run -- acp 2>/dev/null`，`jq '.error.code'` 返回 `-32002`，`jq '.error.message'` 返回 `"Server not initialized"`。
- [S1-C12] initialize 只能调用一次。重复发送 initialize 请求（第二次调用）应返回错误。不应重新初始化或覆盖已建立的会话状态。错误码建议使用 -32002（Server not initialized，表示服务端处于已初始化状态、拒绝重复初始化）。
  - how: 连续发送两个 initialize 请求：`printf '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}\n{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":1}}\n' | cargo run -- acp 2>/dev/null`，`jq -s '.[0]'` 可见正常 initialize response，`jq -s '.[1].error'` 非 null（`jq -s '.[1].error.code'` 返回 `-32002`）。
- [S1-C13] 收到 JSON 合法但 params 非法的 initialize 请求时（如缺少必填字段 `protocolVersion`，或 `protocolVersion` 类型错误），返回 Invalid params 错误（code: -32602, message: "Invalid params"）。
  - how: `echo '{"jsonrpc":"2.0","id":3,"method":"initialize","params":{}}' | cargo run -- acp 2>/dev/null`（缺少 protocolVersion），`jq '.error.code'` 返回 `-32602`。再测 `echo '{"jsonrpc":"2.0","id":4,"method":"initialize","params":{"protocolVersion":"1"}}' | cargo run -- acp 2>/dev/null`（字符串而非 number），`jq '.error.code'` 返回 `-32602`。