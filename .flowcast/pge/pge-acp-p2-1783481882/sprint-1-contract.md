# Contract: Sprint 1 — stdio JSON-RPC loop + initialize 握手 (rev 2)

交付 AcpServer struct + AcpServerRunner::run() stdio 循环，实现 initialize 握手（返回 protocolVersion=1、agentInfo、agentCapabilities）和 initialized notification，未实现方法返回 MethodNotFound(-32601)，非法 JSON 返回 Parse error(-32700)，合法 JSON 但缺少 jsonrpc/method 字段返回 Invalid Request(-32600)。stdin EOF 时 server 正常退出，exit code 0。

## Criteria
- [C1] 发送 initialize request 后，server 返回 JSON-RPC 2.0 response，result 包含 protocolVersion=1、agentInfo.name='recursive'、agentInfo.version（非空 semver 字符串）、agentCapabilities 对象，最小声明为 {"session":{"methods":["new"]},"fs":{"methods":["read","write"]},"mcp":{"methods":[]}}，每个 capability 内可含 "status":"planned" 标记方法在后续 sprint 实现
  - how: echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientInfo":{"name":"test","version":"0.1.0"}}}' | cargo run -- acp，断言 stdout 第一行 JSON 的 result.protocolVersion==1、result.agentInfo.name=='recursive'、result.agentInfo.version 匹配 semver 正则、result.agentCapabilities 包含 session/fs/mcp 三个 key，且 session.methods 含 'new'、fs.methods 含 'read' 和 'write'
- [C2] initialize 成功后，server 主动发出 JSON-RPC notification（无 id 字段），method='initialized'，params 为 null 或空对象，且 notification 在 initialize response 之后、其他任何 response 之前到达
  - how: printf '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientInfo":{"name":"test","version":"0.1.0"}}}\n{"jsonrpc":"2.0","id":2,"method":"ping","params":{}}\n' | cargo run -- acp，断言 stdout 第 1 行为 initialize response（id=1）、第 2 行不含 id 字段且 method=='initialized'、第 3 行为 error response（id=2）；追加的 ping 请求保持 stdin 打开，消除 EOF race
- [C3] server 的 stdio 循环正确处理多条连续 JSON-RPC 请求，按 FIFO 顺序逐条返回对应 response，不丢失、不乱序
  - how: printf '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientInfo":{"name":"test","version":"0.1.0"}}}\n{"jsonrpc":"2.0","id":10,"method":"a","params":{}}\n{"jsonrpc":"2.0","id":20,"method":"b","params":{}}\n{"jsonrpc":"2.0","id":30,"method":"c","params":{}}\n' | cargo run -- acp，断言 stdout 共 5 行：第 1 行 initialize response（id=1）、第 2 行 initialized notification、第 3/4/5 行依次为 id=10/20/30 的 error response（error.code==-32601），顺序严格递增
- [C4] initialize request 和 response 的 Rust struct 有 serde 序列化/反序列化 round-trip 单元测试：构造 InitializeRequest → serde_json::to_string → serde_json::from_str → 断言字段相等；同样覆盖 InitializeResponse 和 AgentCapabilities
  - how: cargo test --lib acp 或 cargo test -- <test_name>，断言存在 #[test] fn test_initialize_roundtrip 和 fn test_initialize_response_roundtrip，两者均通过
- [C5] stdin 输入非法 JSON（如裸字符串 'garbage'）时，server 返回标准 JSON-RPC 2.0 error response：jsonrpc='2.0'、id=null（无法解析 id 时）、error.code=-32700、error.message 包含 'Parse error'，且 server 不 panic、不退出，继续等待并正确处理下一条合法请求
  - how: printf 'garbage\n{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientInfo":{"name":"test","version":"0.1.0"}}}\n' | cargo run -- acp，断言 stdout 第 1 行 error.code==-32700、id==null，第 2 行为合法 initialize response（protocolVersion==1）；两行输出均存在证明 server 在 parse error 后继续运行并正确处理后续请求
- [C6] 发送未注册的 method（如 'session/new' 在 Sprint 1 未实现）时，server 返回 error.code=-32601（Method not found），error.message 含 method 名称，id 与请求 id 匹配
  - how: echo '{"jsonrpc":"2.0","id":42,"method":"session/new","params":{}}' | cargo run -- acp，断言 stdout error.code==-32601、id==42
- [C7] 发送 JSON-RPC notification（无 id 字段的合法请求）时，server 不 panic、不返回任何 response（notification 不应有响应），正常等待下一条输入直至 EOF
  - how: echo '{"jsonrpc":"2.0","method":"someNotification","params":{}}' | cargo run -- acp，断言 stdout 为空（无输出行），进程 exit code 0
- [C8] 发送合法 JSON 但缺少 jsonrpc 或 method 字段的请求（如 {"foo":"bar"}）时，server 返回 error.code=-32600（Invalid Request），error.message 包含 'Invalid Request'，id=null（无法解析 id 时）
  - how: echo '{"foo":"bar"}' | cargo run -- acp，断言 stdout error.code==-32600、error.message 含 'Invalid Request'（大小写不敏感）、id==null
- [C9] 项目可通过 cargo test --workspace 和 cargo clippy --all-targets --all-features -- -D warnings，无编译错误、无 clippy 警告
  - how: 执行 cargo test --workspace && cargo clippy --all-targets --all-features -- -D warnings，断言均返回 0