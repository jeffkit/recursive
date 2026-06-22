# 第3轮代码质量分析

**日期**: 2026-06-21  
**分析轮次**: Round 3  
**分析人**: Cursor AI（claude-sonnet-4.6）

---

## 质量门检查结果

### cargo clippy
```
Finished `dev` profile [unoptimized + debuginfo] target(s) in 9.21s
```
✅ 零警告，干净通过。

### cargo test
```
test result: ok. (所有测试套件均通过)
```
✅ 全部通过，零失败。

---

## 本轮分析覆盖范围

| 区域 | 文件/目录 | 状态 |
|------|-----------|------|
| session 持久化 | `src/session/{lifecycle,writer,reader,orphan,serialize}.rs` | ✅ 已检查 |
| compact 压缩逻辑 | `src/compact.rs` | ✅ 已检查 |
| tools（未检查区域） | `src/tools/{edit,glob,shell,fs}.rs` | ✅ 已检查 |
| 集成测试网络超时 | `tests/{anthropic_smoke,deferred_tool_loading,agui_e2e,mcp_e2e}.rs` | ✅ 已检查 |
| LLM provider 硬编码 | `src/llm/{openai,anthropic,chat,mock}.rs` | ✅ 已检查 |
| 最近 2 个 commit 新增代码 | 见下节 | ✅ 已检查 |

---

## 发现的问题

### P3-001 [LOW] `compact.rs::safe_split_point` — `keep_n=0` 时有越界 panic 风险

**文件**: `src/compact.rs` 第 185–191 行  
**描述**:

```rust
pub fn safe_split_point(transcript: &[Message], keep_n: usize) -> usize {
    let mut split = transcript.len().saturating_sub(keep_n);
    while split > 0 && matches!(transcript[split].role, ...) {  // ← 潜在越界
        split -= 1;
    }
    split
}
```

当 `keep_n = 0` 时：
- `split = transcript.len().saturating_sub(0)` = `transcript.len()`
- 若 transcript 非空，`split > 0` 为 true，紧接着 `transcript[split]` = `transcript[transcript.len()]` → **index out of bounds panic**

`keep_recent_n` 默认值为 8，但函数签名和 `keep_recent_n(n)` builder 方法均未对 `n == 0` 做验证或保护。当前没有针对此路径的测试。

**修复建议**: 在 while 条件中增加 `split < transcript.len()` 检查：
```rust
while split > 0 && split < transcript.len() && matches!(transcript[split].role, ...) {
```

---

### P3-002 [LOW] `compact.rs` — 死变量 `_older_chars` 造成无效计算

**文件**: `src/compact.rs` 第 286 行  
**描述**:

```rust
let _older_chars: usize = older.iter().map(|m| m.content.len()).sum();
```

此变量被 `_` 前缀抑制了 clippy 的 `unused_variable` 警告，但其赋值依然在每次 `compact()` 调用时执行（遍历 older 消息列表累加字符长度），结果永远不被使用。  
下方的 `summary_chars = summary.len()` 才是实际参与 header 格式化的值。

**修复建议**: 直接删除第 286 行，或若将来需要记录「压缩前字符数」再恢复。

---

### P3-003 [LOW] 最近 commit — `config.rs` 新测试遗漏 env 清理

**文件**: `src/config.rs`，测试 `stuck_window_and_error_rate_env_override`（第 1257–1273 行）  
**描述**:

该测试在文件内所有同类测试中是唯一一个**未在函数末尾清理** `RECURSIVE_MODEL` 和 `RECURSIVE_API_KEY` 的。比对同文件其他已有测试（如第 726–727 行、第 963–964 行）均有：

```rust
std::env::remove_var("RECURSIVE_MODEL");
std::env::remove_var("RECURSIVE_API_KEY");
```

但新测试只清理了 `RECURSIVE_STUCK_WINDOW` 和 `RECURSIVE_STUCK_ERROR_RATE`，遗漏了前两个。由于 `_env_lock` 仅序列化测试执行而不恢复环境，`RECURSIVE_MODEL=test-model` / `RECURSIVE_API_KEY=test-key` 会在测试结束后残留，可能污染同测试二进制内后续获取 `env_lock` 的测试（尤其是那些依赖这两个变量"未设置"状态来验证错误路径的测试）。

**修复建议**: 在测试函数末尾（两个 `remove_var` 调用之后）补充：

```rust
std::env::remove_var("RECURSIVE_MODEL");
std::env::remove_var("RECURSIVE_API_KEY");
```

---

### P3-004 [LOW] `tests/anthropic_smoke.rs` / `tests/deferred_tool_loading.rs` — mock 服务器线程无读取超时

**文件**: `tests/anthropic_smoke.rs` 第 59–83 行；`tests/deferred_tool_loading.rs` 第 47–81 行  
**描述**:

两个测试文件均使用 `std::net::TcpListener::bind("127.0.0.1:0")` 搭建 mock HTTP 服务器，内部线程阻塞在 `listener.accept().unwrap()` 和 `stream.read(&mut buf)` 上，均没有设置 socket 读取超时（`TcpStream::set_read_timeout`）。

若 reqwest 客户端因某种原因不发起连接或发送不完整请求，线程会**无限期阻塞**，直至 test runner 超时杀死进程。虽然 loopback 连接实践中极少出现此问题，但这与 AGENTS.md 中"所有网络测试必须设置超时"的原则不符，也使测试在 CI 超时异常时难以调试。

对比：`tests/mcp_e2e.rs` 已正确通过 `McpClient::spawn_with_timeout(...)` 设置超时。

**修复建议**: 在 mock 服务器线程内的 `accept()` 之后为 stream 设置读取超时：
```rust
stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).ok();
```

---

## 已检查、无新问题的区域

| 区域 | 结论 |
|------|------|
| `src/session/lifecycle.rs` — SessionLock、stale lock 恢复、truncate_transcript | 逻辑严密，测试覆盖完整（4 个 lock 测试 + 3 个 truncate 测试） |
| `src/session/writer.rs` — SessionWriter、SessionPersistenceSink | Mutex poison 恢复已测试；bump_updated_at 失败不阻断 agent run；逻辑正确 |
| `src/session/reader.rs` — compact_boundary 跳过逻辑 | `split_off(boundary_after)` 语义正确，边界条件处理无误 |
| `src/compact.rs` — 一般 compact 路径、structured 回退、tool-pair 不拆分 | compact.rs 测试覆盖良好（7 个测试）；唯一问题见 P3-001/P3-002 |
| `src/tools/edit.rs` | 模糊匹配链完整、沙盒隔离、partial-read guard 均已测试；产品代码无 unwrap |
| `src/tools/glob.rs` | 无 panic 路径（`unwrap_or(entry.path())` 是合法回退）；测试覆盖完整 |
| `src/tools/shell.rs` | 子进程 wait 已设置 timeout；read_capped 无 panic；max_output_bytes 截断已实现 |
| `src/tools/fs.rs` | 产品代码无 unwrap/expect；partial read 记录正确 |
| `src/llm/openai.rs` — P2-006 修复验证 | `process_sse_line` 现在正确接受并使用 `provider: &str` 参数，不再硬编码 `"openai"` |
| `src/tools/web_fetch.rs` — P2-005 修复验证 | `crate::truncate_str(&body, max_bytes)` 已替换直接字节切片，测试到位 |
| `src/config.rs` — P2-007 修复验证 | `stuck_window_and_error_rate_env_override` 测试通过 `Config::from_env()` 端到端验证；但见 P3-003 |
| `tests/agui_e2e.rs` | 使用 `tokio::net::TcpListener`（async），`shell_timeout_secs: 5` 已设置 |
| `tests/mcp_e2e.rs` | `TEST_READ_TIMEOUT` 已通过 `spawn_with_timeout` 传入，符合规范 |

---

## 总结

| 严重级别 | 数量 |
|----------|------|
| HIGH     | 0    |
| MEDIUM   | 0    |
| LOW      | 4    |

**发现新问题**: yes（4 个 LOW）

**最值得修复的前 3 个**:

1. **P3-001** — `compact.rs::safe_split_point` 越界 panic（`keep_n=0`）：真实 panic 路径，一行修复，应补充边界测试
2. **P3-003** — `config.rs` 新测试缺少 env 清理：与文件内其他测试的已有惯例不一致，两行修复
3. **P3-002** — `compact.rs` 死变量 `_older_chars`：一行删除，消除无效计算

P3-004（mock 服务器无读取超时）影响最小，可在下次触及该测试文件时顺带修复。
