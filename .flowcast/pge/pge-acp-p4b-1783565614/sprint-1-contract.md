# Contract: Sprint 1 — stdio JSON-RPC loop + initialize（P1） (rev 2)

通过 stdio 启动 ACP server，实现 JSON-RPC 主循环与 initialize 握手：复用 McpServerRunner 的 stdin/stdout 模式但不耦合 MCP 实现，stderr 只打日志、stdout 只走协议，严格遵循 JSON-RPC 2.0 错误码语义与 ACP v1 状态机（uninitialized → initialized），未实现的方法返回 MethodNotFound 而不 panic，notification 不产生响应。

## Criteria
- [AC01] initialize 请求返回正确响应（含 agentInfo.name 与 agentInfo.version）
  - how: echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}' | recursive acp，assert stdout 输出的 JSON-RPC response 包含：`result.protocolVersion == 1`，`result.agentInfo.name == "recursive"`，`result.agentInfo.version` 为非空 semver 字符串（匹配 `^\d+\.\d+\.\d+`），`result.agentCapabilities` 包含所有未来能力字段。
- [AC02] initialize 响应中 agentCapabilities 声明完整
  - how: 解析 AC01 响应的 `result.agentCapabilities`，验证所有 spec 要求的能力均已声明：`session.new`（含 `cwd` 参数支持）、`session.prompt`、`session.cancel`、`session.load`、`session.resume`、`fs.readTextFile`、`fs.writeTextFile`、`mcp`、`permission`。值可为空对象 `{}` 但 key 必须存在。
- [AC03] initialize 之后，非 initialize 请求返回 MethodNotFound（-32601）
  - how: printf '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}\n{"jsonrpc":"2.0","id":2,"method":"session/new","params":{}}\n' | recursive acp，取第 3 行 stdout（initialize response + initialized notification 之后的第一个业务响应），assert 为 JSON-RPC error 对象且 `error.code == -32601`（MethodNotFound），进程退出码为 0。
- [AC04] 未 initialize 状态下收到未知方法不 panic，进程存活且后续仍可正常握手
  - how: printf '{"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{}}\n{"jsonrpc":"2.0","id":2,"method":"foo","params":{}}\n{"jsonrpc":"2.0","id":3,"method":"bar","params":{}}\n{"jsonrpc":"2.0","id":4,"method":"initialize","params":{"protocolVersion":1}}\n' | timeout 5 recursive acp，验证：① 前 3 行各为合法 JSON-RPC error 响应（含 `error.code`）；② 第 4 行（initialize 响应）包含 `result.protocolVersion == 1` 与 `result.agentInfo.name == "recursive"`；③ 进程退出码为 0（非 SIGABRT/coredump）。
- [AC05] stdout 只输出 newline-delimited JSON-RPC 2.0，stderr 只输出日志
  - how: 启动 server 并发送一个 initialize 请求，分别捕获 stdout 和 stderr。验证：① stdout 每一行均为合法 JSON（`jq .` 不报错）且每行均含 `"jsonrpc":"2.0"`；② stderr 包含日志行（含 timestamp/level 模式如 `INFO`）；③ stdout 中无日志噪音（不含 `INFO`/`DEBUG`/`WARN`/`ERROR` 等日志级别标记）。
- [AC06] AcpServerRunner::run() 复用 stdio 模式但不耦合 MCP
  - how: 自动化验证模块边界隔离：① `grep -r 'crate::mcp_server\|use super::mcp\|use crate::mcp_server' src/acp/` 输出为空（acp 模块不引用 mcp_server）；② `grep -r 'crate::acp\|use super::acp' src/mcp_server.rs` 输出为空（mcp_server 不引用 acp）；③ `cargo test --workspace` 通过。
- [AC07] recursive acp 子命令可正常启动并进入监听状态
  - how: 执行 `recursive acp --help` 确认子命令存在且有帮助文本；执行 `timeout 2 recursive acp < /dev/null 2>/dev/null` 确认进程不立即退出（处于阻塞等待 stdin 状态），超时后退出码为 124（timeout kill）而非非零错误退出。
- [AC08] initialize 成功后 server 发送 `initialized` notification
  - how: echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}' | recursive acp，捕获全部 stdout 行，验证其中一行精确匹配 `{"jsonrpc":"2.0","method":"initialized"}`（无 `id` 字段），且该行出现在 initialize response 之后。
- [AC09] 非法 JSON 输入返回 ParseError（-32700）
  - how: echo 'not json' | recursive acp，assert stdout 为 JSON-RPC error 对象，`error.code == -32700`（ParseError），进程退出码为 0（不 panic）。
- [AC10] 缺少 jsonrpc 或 method 字段返回 Invalid Request（-32600）
  - how: 分别发送 ① `{"id":1,"method":"initialize"}`（缺 `jsonrpc`）和 ② `{"jsonrpc":"2.0","id":1}`（缺 `method`），assert 每次响应均为 JSON-RPC error 且 `error.code == -32600`（Invalid Request）。
- [AC11] initialize 参数无效返回 Invalid Params（-32602）
  - how: 分别发送 ① `{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}`（缺 `protocolVersion`）；② `{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"1"}}`（类型错误，应为整数）；③ `{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":99}}`（版本不支持）。每次 assert 响应为 `error.code == -32602`（Invalid Params），进程存活。
- [AC12] initialize 之前任何非 initialize 请求返回 Server not initialized（-32002）
  - how: echo '{"jsonrpc":"2.0","id":1,"method":"session/new","params":{}}' | recursive acp，assert 响应为 JSON-RPC error 且 `error.code == -32002`（Server not initialized），非 -32601（MethodNotFound）。
- [AC13] 重复 initialize 返回错误
  - how: printf '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}\n{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":1}}\n' | recursive acp，验证第一次成功（`result.protocolVersion == 1`），第二次返回 JSON-RPC error（进程不 panic）。
- [AC14] Notification（无 id 字段）不产生任何响应
  - how: echo '{"jsonrpc":"2.0","method":"someNotification","params":{}}' | recursive acp，assert stdout 为空（零行输出），且进程正常退出（退出码 0）。
- [AC15] 代码通过全部质量门
  - how: 依次运行 ① `cargo test --workspace` ② `cargo clippy --all-targets --all-features -- -D warnings` ③ `cargo fmt --all -- --check`，assert 三条命令均退出码为 0，无一告警或失败。
- [AC16] 非 test 代码中不存在 unwrap()/expect()
  - how: `grep -rn '\.unwrap()' src/ --include='*.rs'` 和 `grep -rn '\.expect(' src/ --include='*.rs'`，对每个命中行人工确认仅存在于 `#[cfg(test)]` 块内、`tests/` 目录中、或带有 `// SAFETY:` 注释的已知安全调用点。product 代码路径不得包含未标注的 unwrap/expect。