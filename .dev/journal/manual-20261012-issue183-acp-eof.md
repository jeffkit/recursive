# manual-20261012 — ACP stdin EOF 后空转不退出（issue #183）

- **Date**: 2026-10-12 (Asia/Shanghai)
- **Issue**: https://github.com/jeffkit/recursive/issues/183（P2，属 #190「MCP/ACP 协议与生命周期」簇）
- **Base**: `origin/main` = 6c724056（worktree `fix/183-acp-eof-shutdown`）

## 根因

`src/acp/server.rs:1529-1546` 的 `run_stdio` 循环每次都对 `lines.next_line()` 重新套一个 60s
超时，而 EOF 走的是「睡 100ms 再 continue」：

```rust
Ok(Ok(None)) => {
    // EOF — for /dev/null this happens immediately.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    continue;
}
```

管道一关，`next_line()` 立刻返回 `Ok(None)`，于是循环以 10 次/秒永远空转；`Err(_elapsed)`
那条关停分支在管道已关的连接上**永远不可达**（规格正文写「永不 reachable」略过头：管道仍开着
但长时间无输入时它仍然可达，正是既有两条 idle 用例覆盖的路径）。父进程（编辑器/终端）退出后，
ACP 进程泄漏且持续唤醒。

## 改动

- `src/acp/server.rs`
  - `Ok(Ok(None))` 视为关停信号：打 `ACP stdin closed (EOF), shutting down` 并 `break`。
  - 空闲判定改为**距最后一次输入**（`last_input: tokio::time::Instant`），`Err(_elapsed)` 只在
    `last_input.elapsed() >= 60s` 时关停，否则继续等（不误杀）。
  - 循环退出后统一做收尾：`sessions.kill_all_mcp_children().await`——对端已走，任何 session 都
    不该比它的 stdio 管道活得久（`kill_mcp_children` 的文档早就写了「client disconnect cleanup」，
    但此前没有调用点）。
  - 更新 `run_stdio` 文档注释（原文写的是「EOF 只会 sleep 后重试，服务器不会因 EOF 退出」）。
  - 新增 2 条用例：`eof_shuts_down_server`（关 writer → `run_stdio` 必须返回；用真实时间，
    因为失败形态就是挂死）、`idle_timer_resets_on_input`（t=50s 来一行 → 再买 60s）。
- `src/acp/session.rs`
  - 新增 `AcpSessionManager::kill_all_mcp_children()`（对所有 session 调 `kill_mcp_children`），
    补 1 条用例 `kill_all_mcp_children_clears_every_session`。这是 #183 要求的「清理点」。

## 验证

- `cargo test -p recursive-agent --lib acp::` → **195 passed**（含 3 条新增）
- `cargo test -p recursive-agent --lib` → **3302 passed / 0 failed**
- `cargo test --test invariants` → **55 passed**
- `cargo clippy -p recursive-agent --lib -- -D warnings`、`cargo fmt --all -- --check` 干净
- **负向对照**（证明新断言真能抓到这个 bug）：把 `src/acp/server.rs` 单独换回 `origin/main`
  版本（新用例保留），`eof_shuts_down_server` **转红**：
  `panicked: run_stdio must return after stdin EOF (#183)`；随后还原并按 md5 校验一致。
- 既有两条 idle 用例（60s 关停 / 不到 60s 不关停）保持绿 → 「管道仍开但静默」的语义未被改动。

## Notes

- `kill_all_mcp_children` 用的是既有 kill 协议（SIGTERM → grace → SIGKILL）；用例里用平台范围外的
  pid，`kill`/`wait` 会往 stderr 打 `No such process` 噪声（预期，非失败）。若时钟处于 `pause()`
  且真有被跟踪的 pid，grace 等待会跟着暂停时钟——只影响测试写法，不影响生产路径。
- 未做（规格范围外）：`--allow-tools` / `mcpServers` 接线（#180）、`session/cancel` 打断（#179）、
  ACP 其余生命周期；本单只改关停路径。
- 与 #179/#180 的关系：它们都 `depends-on: #183`，因此本单落地后那两单才具备「transport 会真正关停」
  的前提（本单不改它们的实现）。
