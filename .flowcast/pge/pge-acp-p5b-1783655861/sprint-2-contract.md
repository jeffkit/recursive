# Contract: Sprint 2 — Editor 反向 fs + MCP 多 transport + session 生命周期清理 (rev 2)

在 Sprint 1 已完成的 permission 通道上叠加两个垂直能力：editor→agent 反向文件系统（client 声明 fs.readTextFile=true 时 Read 工具优先请求 client 未保存 buffer，Write 通过 ClientWriteFile 写回）和 MCP 三种传输协议（stdio/http/SSE）全覆盖。MCP bridge 新增核心路由层：agent tool_call 根据 tool_name 路由到正确的 MCP server，server 返回结果回填 tool_result。三种传输的连接失败（5xx/404/stdio crash）纳入错误处理，session 以 degraded 状态继续运行而不整体失败。session/close 必须 kill stdio 子进程防泄露，kill-gracefully 三阶段逻辑封装为可测试独立函数，且 ACP client 断连时自动触发 MCP 子进程清理。ClientReadFile/ClientWriteFile 在 tools/mod.rs 注册并暴露 tool_spec。沙箱校验在两条路径上都跑。session 配置与全局配置采用 shadow（完全覆盖）策略。

## Criteria
- [S2-E1] ACP client 声明 fs.readTextFile=true 后，agent 的 Read 工具优先发出 ClientReadFile 请求获取 client 端的未保存 buffer 内容
  - how: 编写 test: 启动 ACP session 时 client 声明 capabilities.fs.readTextFile=true，向 agent 发送 prompt 要求读取某文件。验证 agent 发出的 tool_call 中应首先包含名为 ClientReadFile 的工具调用，且参数 uri 指向目标文件。mock client 返回 buffer 内容，验证 agent 将 buffer 内容纳入后续回复，未触发本地 filesystem Read。
- [S2-E2] 当 ClientReadFile 返回错误或 client 超时不回应（超时阈值 5s，可通过配置项 acp.client_read_timeout_ms 调整）时，Read 工具降级为本地 filesystem Read 并始终经过 sandbox resolve_within
  - how: 编写 test: 同 S2-E1 场景但 mock client 返回 ClientReadFile 错误或超过 5s 不回应。验证 agent 在失败后降级调用本地 Read 工具，且 Read 参数的 path 已通过 tools::resolve_within 约束在 sandbox 内。assert 日志中出现 'fallback to local read' 标记。配置 acp.client_read_timeout_ms=100 验证超时行为可在毫秒级生效。
- [S2-E3] ACP client 未声明 fs.readTextFile（或声明 false）时，agent 的 Read 工具不发送 ClientReadFile，直接使用本地 filesystem Read 并经过 resolve_within
  - how: 编写 test: ACP session 初始化时 client 不设置 capabilities.fs.readTextFile（或设为 false）。发送 prompt 要求读取某文件。验证 agent 的 tool_call 序列中不出现 ClientReadFile 工具名，所有 Read 调用的 path 经过 resolve_within 约束。
- [S2-E4] ACP client 声明 fs.writeTextFile=true 后，agent 的 Write 工具通过 ClientWriteFile 将内容写回 client buffer
  - how: 编写 test: ACP session 中 client 声明 capabilities.fs.writeTextFile=true。agent 执行 Write 工具修改某文件。验证 agent 发出的 tool_call 序列包含 ClientWriteFile（参数含 file_path 和 content），不包含本地的 Write 工具调用。mock client 确认接收后，assert 磁盘上目标文件内容未改变（确认数据仅写入 client buffer）。
- [S2-E5] 即使在 ClientWriteFile 路径上，沙箱逃逸检测依然触发：bridge 在发出 ClientWriteFile 之前，必须先将 file_path 参数通过 tools::resolve_within 校验，未通过则拒绝发出 ClientWriteFile
  - how: 编写 test: 同 S2-E4 场景，但请求写文件路径在 sandbox 之外（如 /etc/passwd）。验证 bridge 在发出 ClientWriteFile 之前调用 resolve_within 并判定 path 越界。验证 agent 返回 permission_denied 错误或拒绝执行，ClientWriteFile 不被发出，且日志包含沙箱逃逸警告（含 resolve_within 拒绝标记）。
- [S2-E6] session/new 的 mcpServers 支持 stdio 传输：配置 server.transport='stdio' 时 MCP bridge 启动子进程建立连接
  - how: 编写 test: ACP session 初始化时传入 mcpServers 包含一个 transport='stdio' 的 server 配置，command 设为可验证的工具。verify 该 server 已连接（session/mcpServerList 返回 ready）。验证存在该 server 的子进程（PID > 0），bridge 通过 stdin/stdout 与之通信。
- [S2-E7] session/new 的 mcpServers 支持 http 传输：配置 server.transport='http' 时 MCP bridge 通过 HTTP POST 连接远程 server
  - how: 编写 test: 启动一个 HTTP MCP server（test-only mock），ACP session 初始化时传入 mcpServers 包含 transport='http'，url 指向 mock server 端点。验证 bridge 成功完成 initialize 握手（发送 JSON-RPC POST /messages），server 状态变为 connected。
- [S2-E8] session/new 的 mcpServers 支持 SSE 传输：配置 server.transport='sse' 时 MCP bridge 通过 SSE stream 建立连接
  - how: 编写 test: 启动一个 SSE MCP server（test-only mock），ACP session 初始化时传入 mcpServers 包含 transport='sse'，url 指向 SSE 端点。验证 bridge 通过 EventSource/SSE 接收 server→client 消息，通过单独的 HTTP POST endpoint 发送 client→server 消息，状态变为 connected。
- [S2-E9] MCP bridge 根据 server.transport 字段分路：stdio 走子进程管理，http/sse 走 HTTP 客户端，不混用
  - how: 编写 test: 构造一个包含三种 transport 混合的 mcpServers 配置（stdio + http + SSE 各一）。验证每个 server 使用了对应的连接策略：stdio 的 stdin/stdout 通信，http 的 POST-only，SSE 的 EventSource+POST 组合。三个 server 同时 connected。
- [S2-E10] mcpServers 中与全局 MCP 配置命名冲突时，session-scoped 配置采用 shadow（完全覆盖）策略，整体替换而非 deep merge——session 配置中未声明的字段不继承全局
  - how: 编写 test: 全局 MCP 配置中定义 server 'my-tools' 含 command、args、env（含 KEY_A=global_a）。session 初始化时 mcpServers 也包含同名 'my-tools' 但仅指定 command（不含 env）。验证 bridge 使用 session 配置的 command 连接，且 env 为空（不继承 KEY_A=global_a）。验证全局 'my-tools' 未被修改，其他 session 仍可访问全局配置。
- [S2-E11] session/close 时 kill 所有此 session 启动的 stdio MCP 子进程（SIGTERM → 超时 → SIGKILL），无僵尸进程残留
  - how: 编写 test: session 中启动一个 stdio MCP server，记录其 PID。调用 session/close。验证：1) 先发送 SIGTERM；2) 若进程在 N 秒（可配置，默认 5s）内未退出则发送 SIGKILL；3) 最终进程不存在（kill -0 返回非0）；4) waitpid 已收割，非僵尸。assert 日志包含 'sent SIGTERM' 和 'sent SIGKILL' 或 'process exited gracefully'。
- [S2-E12] session/close 仅 kill 此 session 启动的子进程，不影响其他 session 或全局 MCP server
  - how: 编写 test: 创建两个独立 session，各启动一个 stdio MCP server。关闭 session A。验证 session A 的 server 进程已死，session B 的 server 进程仍在运行且功能正常。
- [S2-E13] session/load 时触发老 MCP server kill（SIGTERM → 超时 → SIGKILL）后启动新 server，与 Sprint 1 的 MCP 重连逻辑同一套 kill-gracefully 机制
  - how: 编写 test: 初始 session 含 stdio MCP server A。调用 session/load 传入新 mcpServers 配置（不同 command）。验证 bridge 先 kill 旧 server A（同 S2-E11 协议），然后启动新 server B。assert 日志中出现相同 kill-gracefully 函数名。
- [S2-E14] session/resume 时触发老 MCP server kill 后启动新 server，与 load 共享同一 kill-gracefully 函数
  - how: 编写 test: 同 S2-E13 但使用 session/resume。验证 kill 逻辑与 load 完全一致，调用同一个封装函数或方法。assert 代码路径证明 load 和 resume 均调用该函数而非内联重复逻辑。
- [S2-E15] kill 子进程的 graceful-timeout-kill 三阶段逻辑被封装为可测试的独立函数，不依赖 session 生命周期代码
  - how: 在独立单元测试中直接调用该函数：传入 mock 子进程（或真实子进程）。验证：阶段1发送 SIGTERM；阶段2等待 grace period（可配置）；阶段3若进程仍存活则 SIGKILL。函数返回 killed_by（graceful/timeout/kill）。测试不依赖完整的 session 创建/关闭流程。
- [S2-E16] kill-gracefully 函数被所有 MCP 子进程关闭路径（session/close、session/load、session/resume、agent 崩溃清理、ACP client 断连清理）调用，不会因遗漏而泄露
  - how: 代码审查 + grep: 搜索所有子进程 kill/terminate 调用点，验证均通过该独立函数而非直接 system::kill。在单元测试中 mock 子进程管理器，调用所有关闭路径，assert 该函数被调用次数与预期一致。
- [S2-E17] MCP bridge 必须包含工具调用路由层：agent 发出 tool_call 后，bridge 根据 tool_name 的 server 前缀或映射表路由到对应 MCP server（stdio/http/SSE），server 返回 results 后 bridge 回填到 tool_result 返回 agent；无匹配 server 的 tool_name 不转发
  - how: 编写 test: 注册两个 MCP 服务器（stdio+HTTP 各一），工具集不重叠（stdio 提供 'calc_add'，HTTP 提供 'fetch_title'）。agent 发出 tool_call { name: 'calc_add', args: { a:1, b:2 } }。验证 bridge 仅转发到 stdio server（不转发到 HTTP server），stdio server 返回结果，agent 收到 tool_result 含结果 3。再发出 tool_call { name: 'fetch_title', args: { url:'...' } }，验证仅转发到 HTTP server。assert 日志记录路由决策 'route tool X to server Y'。对于不匹配任何 MCP server 的 tool_name（如原生 Read），验证 bridge 不转发。
- [S2-E18] 传输层连接失败不阻断整个 session/new 创建：当 HTTP MCP server 返回 5xx、SSE endpoint 返回 404 或 stdio binary 启动即崩溃时，session 应进入 degraded 状态（已创建但失败 server 标记 error），其余 server 仍尝试连接，无遗留子进程
  - how: 为每种传输编写独立 test: (a) HTTP server 在 /messages 返回 500；(b) SSE url 返回 404 非 JSON-RPC；(c) stdio command 为不存在的 binary 或立即 exit non-zero。每种场景验证：session/new 不整体失败（返回 session id），session/mcpServerList 中该 server 状态为 error（含错误消息），其他 server（若有）仍 connected。ps/进程检查确认无孤儿子进程残留。再运行一次正常 session 证明全局状态未受损。
- [S2-E19] ACP client 在 session 进行中主动断连（WebSocket close / HTTP 连接断开）时，session 自动触发所有 MCP 子进程清理，调用 kill-gracefully 函数，不泄露进程
  - how: 编写 test: 启动 session，创建 stdio MCP server，验证子进程活跃。模拟 ACP client 断连（关闭 WebSocket 连接或 HTTP stream 断开）。验证：同一 kill-gracefully 函数被触发，MCP 子进程收到 SIGTERM→SIGKILL 序列（若未及时退出），进程被收割（非僵尸）。验证其他不相关 session 的 MCP server 不受影响。assert 日志包含 kill-gracefully 调用标记与 'client disconnected, cleaning up MCP servers'。
- [S2-E20] ClientReadFile 和 ClientWriteFile 在 tools/mod.rs 注册为有效工具并暴露 tool_spec，description 明确指导 LLM 的使用时机，仅在 ACP session 活跃时可见
  - how: 代码审查: grep tools/mod.rs 确认存在 ClientReadFile 和 ClientWriteFile 的注册条目。验证 tool_spec 的 description 包含 'use this when the session client has declared fs.readTextFile=true / fs.writeTextFile=true' 之类指引。参数 schema 正确：ClientReadFile 含 uri（string），ClientWriteFile 含 file_path（string）+ content（string）。编写 test: 在 ACP session 中调用 tools/list，验证返回列表包含 ClientReadFile 和 ClientWriteFile；在非 ACP session（普通 agent 会话）中调用 tools/list，验证不包含 ClientReadFile/ClientWriteFile。