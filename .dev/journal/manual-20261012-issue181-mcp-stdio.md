# Issue #181 — MCP stdio 客户端：`tools/call` 硬编码 10s 超时、丢弃服务端请求、能力判定误判

- **Date**: 2026-10-12
- **Base**: `origin/main` @ `6c724056`（worktree `fix/181-mcp-stdio-lifecycle`，本地提交，未推送）
- **Goal**: issue #181 的四条规格（超时分级 / 服务端请求答复 / 能力判定 / 非 JSON 行跳过）
- **Verdict**: completed

## 根因（先核对，再动手）

在 `6c724056` 上逐条复现，四条**行为**claim 全部成立，行号有漂移：

1. `McpClient::spawn`（base `src/mcp.rs:387`）无条件传
   `DEFAULT_STDIO_READ_TIMEOUT`（10s），该值被 `read_response` 用于**所有**请求。
   `call_tool` 无任何超时通道 → 任何 >10s 的工具调用必定失败，且调用方不可配。
   `src/mcp_server.rs:309` 与 `crates/recursive-cli/src/cli/builder.rs:500` 都走
   `McpClient::spawn`，两条生产路径同时中招（规格「陷阱」第 2 条确认成立）。
2. `read_stdio_response`（base `1098-1200`）只认 `id == expected_id` 的响应：
   服务端发起的**请求**（有 `id` 且有 `method`，如 `ping`）既不回 `{}` 也不回
   错误，被静默丢弃；对端会一直等自己的超时。
3. 同一函数里任何非 JSON 行都走 `map_err` 直接 `return Err`（base `1156-1159`），
   把整个待决调用打断 —— 实测红线：
   `server returned non-JSON line: expected value at line 1 column 2;
   line: [mock] progress: handling initialize`。服务端往 stdout 打一行进度就废。
4. 能力字段一律 `as_bool()`（base `645-653`）。而 MCP 规范的形状是**对象**
   （`"resources": {}`；本项目自己的 `src/mcp_server.rs:134` 就是 `"tools": {}`），
   于是 `list_resources`（base `795`）/`list_prompts`（base `864`）对任何
   规范服务端恒报 `does not advertise`。

## 改动

全部集中在 `src/mcp.rs`，外加测试夹具；生产调用方（CLI / `mcp_server.rs`）**零改动**
即自动受益，因为修复挂在它们共用的 `McpClient::spawn` + `call_tool` 上。

1. **超时分级**（`src/mcp.rs:360`、`375-380`、`385-403`、`449-479`、`509-537`、
   `540-560`、`820-825`、`837-849`）
   - 新字段 `McpClient::tool_timeout`，与 `read_timeout` 分开：
     `read_timeout` 继续管 `initialize` / `tools/list`（10s），`tool_timeout`
     只管 `tools/call`。
   - 新常量 `DEFAULT_MCP_TOOL_TIMEOUT = 300s`；新公开环境变量开关
     `MCP_TOOL_TIMEOUT_ENV = "RECURSIVE_MCP_TOOL_TIMEOUT_SECS"`，由
     `default_tool_timeout()`（私有）解析，非法/`0` 值 warn 后回落默认。
   - 新公开 builder `McpClient::with_tool_timeout(Duration)`（`840`）供库调用方覆盖。
   - `call_tool` 用 `std::mem::replace` 把 `read_timeout` 临时换成 `tool_timeout`，
     请求结束（成功或失败）后原地换回 —— **不做全局放宽**：`initialize` 仍是
     10s，"服务端根本没起" 仍在秒级暴露。
   - `spawn` / `spawn_with_timeout` / 私有 `spawn_with_timeout_and_tool_timeout`
     把两个超时分别下发到 stdio 与两种 HTTP transport。HTTP 客户端
     `build_http_client` 的 reqwest 超时改为 `tool_timeout.max(30s)`
     （原为硬编码 30s），否则远端 300s 工具仍会被 reqwest 自己砍掉。
2. **答复服务端请求**（`src/mcp.rs:1307-1324`、`1375-1405`）
   - `read_stdio_response` 新增分支：带 `id` **且**带 `method` 的消息识别为
     服务端请求 → 新私有 `answer_server_request(stdin, id, method)`：
     `ping` 回 `{"result":{}}`，其余方法回 JSON-RPC `-32601`；`id` 为 null
     （通知）不回。
   - 为此 `read_stdio_response` 增加 `stdin: &mut ChildStdin` 形参，
     `read_response` 的 stdio 分支同时借出 `stdin` 与 `reader`（两个不相交字段，
     借用检查通过）。
   - 写回失败只 `debug!` 记录并继续等目标响应，不打断待决调用。
3. **能力判定**（`src/mcp.rs:135-148`、`748-750`）
   - 新私有 `has_capability(Option<&Value>) -> bool`：
     `Bool(true) | Object(_) | Array(_)` 均视为"已声明"；`false`/`null`/缺省为否。
   - `do_initialize` 三处 `as_bool()` 换成它，旧布尔形态继续兼容。
4. **非 JSON 行跳过**（`src/mcp.rs:1289-1305`）
   - `serde_json::from_str` 失败改为 `tracing::debug!`（记录 server/error/line）
     后 `continue`。安全性：真正的响应必然是合法 JSON，因此跳过不可能吞掉响应。

## 验证

### 红（未打补丁，逐条实测）

新增 7 个集成用例（`tests/mcp_e2e.rs`）+ 3 个单测（`src/mcp.rs`），在**未打补丁**的
`src/mcp.rs` 上运行。为了不让红态停在编译错误，先用一个 trivial
`with_tool_timeout(self, _: Duration) -> Self`（只返回 `self`）垫住链接；失败因此是
**行为性**的：

```
test object_capability_advertises_resources_and_prompts ... FAILED
  object-form `resources` capability must be advertised:
  Mcp { server: "mock", message: "server does not advertise `resources` capability" }
test non_json_stdout_line_is_skipped ... FAILED
  spawn mock server: Mcp { server: "mock", message: "server returned non-JSON line:
  expected value at line 1 column 2; line: [mock] progress: handling initialize" }
test server_ping_is_answered ... FAILED   (mock 未收到任何 ping 答复)
test unknown_server_request_gets_method_not_found ... FAILED   (同上)
test tool_call_respects_configured_timeout ... FAILED
  a 12s tools/call must survive when the tool timeout is 30s:
  Mcp { server: "mock", message: "server timed out (no response within 0s)" }
test tool_call_default_timeout_exceeds_ten_seconds ... FAILED
  default tool timeout must be well above the old hard-coded 10s:
  Mcp { server: "mock", message: "server timed out (no response within 2s)" }
test result: FAILED. 1 passed; 4 failed   (第 5 个是回归守卫 boolean_capability_still_advertises，红态即应为绿)
```

### 负控制（每个新断言都反向证明过）

临时把对应 hunk 还原成修复前形态 → 只有对应用例转红 → 还原 → **`sha256` 与
`git diff` 双重校验回到负控制前的字节**（`src/mcp.rs`
`4a986de503a910d686bad784353bcfe59056ca59a5b2bac4c85ed706d1cd53cc`，
`git diff` 与负控制前逐字节一致，全仓无残留 `NEGATIVE CONTROL` 字样）：

| 负控制 | 还原的 hunk | 转红的用例 | 实测输出 |
|---|---|---|---|
| nc1 | `call_tool` 的超时切换（退化为 `let previous = self.read_timeout;`） | `tool_call_respects_configured_timeout` | `server timed out (no response within 0s)` |
| nc2a | 非 JSON 行 `continue`（退回 `map_err(...)?`） | `non_json_stdout_line_is_skipped` | `server returned non-JSON line: ... line: [mock] progress: handling initialize` |
| nc2b | 服务端请求分支（退回静默丢弃） | `server_ping_is_answered` / `unknown_server_request_gets_method_not_found` | 两者均在 10s 轮询后得到空 marker 文件（服务端从未收到答复） |
| nc3 | `has_capability(...)`（退回三处 `as_bool()`） | `object_capability_advertises_resources_and_prompts` | `server does not advertise \`resources\` capability`；同时 `boolean_capability_still_advertises` 仍绿 |
| nc4 | `has_capability` 本体（退化为只认 `Bool(true)`） | `has_capability_accepts_object_true_and_array` | `assertion failed: has_capability(Some(&serde_json::json!({})))` |

### 绿灯门禁（全部在还原后的最终树上重跑）

| 门禁 | 结果 |
|---|---|
| `cargo test -p recursive-agent --lib mcp` | ok. **74 passed**; 0 failed |
| `cargo test -p recursive-agent --test mcp_e2e` | ok. **22 passed**; 0 failed; 2 ignored |
| `cargo test -p recursive-agent --test mcp_e2e -- --ignored` | ok. **2 passed**（两个 >10s 用例，~12s） |
| `cargo test -p recursive-agent --test mcp_integration` | ok. 0 passed; **4 ignored**（该 base 上 4 个用例全部 `#[ignore]`，需要 `npx` + 网络，未联网运行） |
| `cargo test -p recursive-agent --lib` | ok. **3302 passed**; 0 failed |
| `cargo test --test invariants` | ok. **55 passed**; 0 failed（含 #4 `pub_fns_have_corresponding_tests`、#6 `dep_check_script_passes`） |
| `cargo clippy -p recursive-agent --lib -- -D warnings` | clean，exit 0 |
| `cargo clippy -p recursive-agent --all-targets -- -D warnings` | clean，exit 0（额外自查，非要求） |
| `cargo fmt --all -- --check` | clean，exit 0 |
| `cargo check --workspace --all-targets` | clean（额外自查：cli/tui 等下游消费方均编译通过） |

## Review 返工（第二 commit）：HTTP 侧不再全局放宽握手超时

独立验收给出 **PASS-WITH-NITS**，其中 NIT-1 是真回归、且直接撞规格「陷阱」第 1 条：
第一版把 `build_http_client` 的 **client 级** reqwest 超时从固定 30s 改成
`tool_timeout.max(30s)`。验收者用挂死 loopback + `RECURSIVE_MCP_TOOL_TIMEOUT_SECS=40`
实测 `initialize` 到 **40.004s** 才失败（修复前固定 30s；默认 300s 时"服务端没起"
要 5 分钟才暴露）。stdio 侧分层是对的，只有 HTTP 侧要收回。

### 改法

- `build_http_client` 恢复**固定有界**超时：新常量
  `HTTP_DEFAULT_REQUEST_TIMEOUT = 30s`，**不再**看 `tool_timeout`。
- 新增私有 `McpClient::http_request_timeout(&Value) -> Duration`：逐请求分级 ——
  `tools/call` → `tool_timeout`，其余（`initialize` / `tools/list` / 通知）→
  `read_timeout`。
- `write_line` 里对两种 HTTP transport 的 POST 都加
  `RequestBuilder::timeout(self.http_request_timeout(value))`（reqwest 支持逐请求
  超时，且覆盖 client 级）。这样 `initialize` 仍然快速失败，只有 `tools/call`
  拿到可配置预算。
- **顺带修掉 NIT-2**：`spawn_streamable` / `spawn_http_sse` 此前把
  `read_timeout` 写死成 `DEFAULT_STDIO_READ_TIMEOUT`、完全忽略调用方实参，所以
  `spawn_with_timeout` 对 HTTP 形同虚设（这也正是 `with_tool_timeout` 在 HTTP 上
  静默 no-op 的一半原因）。现在两个 spawn 都收 `read_timeout` 并写进 client，
  `with_tool_timeout` 因此在 HTTP 上真实生效，doc 也明确写了 "Applies to both
  transports ... not stdio-only"。
- **NIT-4**：`tests/bin/mock_mcp_server.rs` 的头表说 `nonjson` 声明 `tools` 能力，
  但 `dispatch` 的 `_ => json!({})` 没给。按头表改代码：
  `"tools-only" | "nonjson" => json!({"tools": true})`。

### 实测数字（hermetic，本地 loopback 慢速 HTTP mock，无外网）

| 场景 | 配置 | 结果 |
|---|---|---|
| `initialize` 快速失败 | mock 延迟 1.2s，read=300ms，tool=5s | **304.7ms** 失败（第一版 client 级超时下是 30s；放宽版是 1.2s+ 才成功） |
| `tools/call` 拿到预算（成功侧） | mock 延迟 800ms，read=300ms，tool=5s | **803.4ms** 成功（若用 read 预算会在 300ms 失败） |
| `tools/call` 预算**有约束力** | mock 延迟 2.5s，read=5s，tool=700ms | **702.4ms** 被切断（若退化成 30s client 级 backstop，调用会成功） |

### 返工负控制

| 负控制 | 还原的 hunk | 转红用例 | 实测输出 |
|---|---|---|---|
| nc5 | 去掉两种 HTTP transport 的 `RequestBuilder::timeout(...)` | `http_initialize_fails_fast_and_ignores_the_tool_budget` + `http_tools_call_is_cut_off_by_a_short_configured_budget` | `a 1.2s initialize must fail under a 300ms budget`；`http tools/call cut off after 2.502019916s` → `a 2.5s tools/call must be cut off by a 700ms tool budget`（即退化到 30s client 级 backstop，预算完全没施加） |
| nc6 | `http_request_timeout` 退化为恒定 `self.read_timeout`（预修复语义：工具预算永不施加） | `http_request_timeout_is_tiered` + `http_initialize_fails_fast_...` | `left: 10s / right: 300s`；`a 1.2s initialize must fail under a 300ms budget` |

还原方式：负控制前把 `src/mcp.rs` 另存副本，验证后整文件写回，
`sha256 = 191db8277fe788839da94c46fb183c288c577dc252346fa7e89c297fe54c9e01`
与保存前一致，`grep -c "NEGATIVE CONTROL"` = 0。注：nc5 的第一轮只让
`http_initialize_...` 转红，因为当时"成功侧"用例会被 30s backstop 掩盖；补上
"约束力侧"用例（2.5s mock vs 700ms 预算）后，nc5 才同时打掉两条 —— 这本身就是
夹具覆盖面的一次补强。

### 新测试

- `mcp::tests::http_request_timeout_is_tiered`（单测，纯函数策略：四类方法 + 通知
  + 短 read 预算 + `tools/call` 低于/高于 read 预算两种情形）
- `mcp::tests::http_initialize_fails_fast_and_ignores_the_tool_budget`（端到端，
  hermetic loopback mock）
- `mcp::tests::http_tools_call_uses_the_configured_tool_budget`（端到端，成功侧）
- `mcp::tests::http_tools_call_is_cut_off_by_a_short_configured_budget`（端到端，
  **约束力**侧：700ms 预算切断 2.5s 调用；这一条在去掉 per-request timeout 时会转红，
  补上了"成功侧会被 30s backstop 掩盖"的漏洞）

## 规格漂移（实测）
规格给的行号整体前移 1~2 行，**行为判断无一处错误**：

| 规格 | base 实际 | 结论 |
|---|---|---|
| `src/mcp.rs:340` 超时常量 | `340` | 未漂移 |
| `:388` spawn | `387` | 漂移 1 |
| `:645-653` `as_bool` | `645-653` | 未漂移 |
| `:792` / `:861` "does not advertise" | `795` / `864` | 漂移 3 |
| `:956-969`（表述为 "stdout 非 JSON 行中断调用"） | 实际在 `read_stdio_response` 内，`1156-1159` | 漂移 ~200；规格括注 `:1131-1200` 是准的 |
| `src/mcp_server.rs:134` 的 `{}` 能力 | `134` 属实（`"tools": {}`） | 成立 |

## Notes / 已知限制

- **不做全局放宽**（规格「陷阱」第 1 条）：`initialize` / `tools/list` 仍是 10s（stdio）
  或 30s（HTTP reqwest 下限），只有 `tools/call` 拿到 300s。`server_timeout_errors_cleanly`
  与 `malformed_server_errors_cleanly` 两个既有用例在修复后仍绿，等于反向确认没有放宽。
- **HTTP 侧超时（返工后）**：两种 HTTP transport 现在都用逐请求
  `RequestBuilder::timeout`：`tools/call` → `tool_timeout`，其余 → `read_timeout`；
  client 级固定 30s 只当backstop。但 **服务端发起的请求仍只在 stdio 得到答复** ——
  HTTP 是"一问一答的 POST"，同一个 pending POST 之外没有可重入的读取点，要支持
  就得改成并发读流（超出本单范围，issue #181 的标题范围也正是 stdio）。
- **NIT-3 记清楚（不改代码）**：这里的 "tool timeout" 是**逐行 idle 超时**，不是
  墙钟 deadline。`read_stdio_response` 每读一行重新起计时，所以服务端只要在预算内
  偶尔吐一行（哪怕是非 JSON 的垃圾行 —— 规格要求跳过它们）就能让调用活过预算；
  验收者探针实测：每 150ms 一行垃圾可让 400ms 预算的调用活到 **1.543s**。这是
  "跳过非 JSON 行" 的固有语义，也是修复前就存在的行为（修复前该行为是直接报错，
  所以不是本次引入的）。HTTP 侧的逐请求超时是真正的墙钟 deadline，两transport
  语义因此在这一点上不同 —— 已知悉，未改。
- **配置入口的选择**：规格说 `spawn` 加 `with_tool_timeout` 让 CLI/服务端透传。
  我没有给 `McpServer`/`McpServerConfig` 加字段（会波及 10+ 处结构体字面量，且
  `McpServerConfig` 是 `.mcp.json` 的 serde 契约，加字段等于扩协议面）。改为：
  公开 builder `with_tool_timeout(Duration)`（库调用方，两种 transport 都生效）+
  环境变量 `RECURSIVE_MCP_TOOL_TIMEOUT_SECS`（CLI 与服务端免改代码即生效，`spawn`
  读取一次）。两条生产路径都实测走 `McpClient::spawn`，因此都覆盖到了。
- **未验证**：`tests/mcp_integration.rs` 的 4 个用例在该 base 上全是 `#[ignore]`，
  需要 `npx` + 网络，未联网运行（编译通过）。本地无网络环境，未跑 e2e 容器门禁
  （且本任务明确不要求、也不运行任何 push 类脚本）。
- **夹具自身的四个坑**（都修掉并写进注释，避免后人重踩）：
  1. `TempDir` 必须在 mock 写 marker 时仍然存活 —— `create(true)` 不会创建父目录，
     提前 `drop(dir)` 会让 mock 静默写不进去（第一次实现就栽在这里，表现为
     "marker 永远为空" 的假红）。
  2. 轮询 marker 时不能用前缀（`SERVER_PING`）当 needle —— 会命中更早写入的行而
     提前返回，把断言静默变成 no-op（假绿）。现在一律轮询完整判词
     （`SERVER_PING_OK`），并留下了注释说明。
  3. 第一版把 mock **独自** spawn（不接 client）再看它 stderr —— 那样**根本没有
     client 参与**，断言是空转的。正确做法是让真实的 `McpClient` 去连 mock
     （`spawn_with_timeout` 会驱动 `initialize` 那次 `read_response`，探针正是在
     那里被消费的），mock 把判词写进 `--marker-file`；helper 把 client 与
     TempDir 一并交还调用方，保证 mock 活到判词被读到为止（不用
     `mem::forget` 泄漏）。
  4. HTTP 慢速 mock **不能**用 `read_to_end` 读请求：reqwest 在等响应期间保持
     请求侧不关，`read_to_end` 会一直阻塞到客户端自己的超时才返回 —— 表现为
     "服务端永远不回、每个用例都跑满超时"的假红。改为读 head 取
     `Content-Length` 再用 `read_exact` 读 body。另外只延迟 `tools/call`
     （`initialize` 与 `notifications/initialized` 立即回），否则通知那次 POST
     也会撞上 read 预算，掩盖真正要测的 tier。
- **依赖**（invariant #6）：零新增。仅用 `std::time::Duration`、`std::mem::replace`、
  `tracing`、既有 `tokio` / `serde_json`；测试夹具复用既有 `tempfile` dev-dependency。
- **invariant 自查**：#1/#2/#3 未触及；#4 新增 5 个单测（3 个 #181 + 2 个 review
  返工）与 7 个集成用例，新公开 API（`with_tool_timeout`、`DEFAULT_MCP_TOOL_TIMEOUT`、
  `MCP_TOOL_TIMEOUT_ENV`、`HTTP_DEFAULT_REQUEST_TIMEOUT`）都有测试；#5 非测试代码无
  `unwrap()`/`expect()`（`default_tool_timeout` 用 `match`，
  `answer_server_request` 返回 `io::Result`）；#6 无新依赖；#7-#10 不涉及。
- **未推送、未在 GitHub 上留任何痕迹**：无 `git push` / `gh pr create` /
  `gh gist create` / issue 评论 / `gh api` 写操作。
