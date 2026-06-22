# Manual edit: quality-fixes-r4

**Date**: 2026-06-22  
**Goal**: Fix three bugs identified in code audit round 4 (HIGH-1, HIGH-2, MEDIUM-2)  
**Files touched**:
- `src/weixin/daemon.rs`
- `src/compact.rs`
- `src/checkpoint.rs`
- `tests/integration.rs`

**Tests added**: Updated `hooks_and_compaction` integration test (6 mock completions instead of 5)

---

## HIGH-1: UTF-8 字节切片 panic（weixin/daemon.rs）

**问题**：`daemon.rs:168` 用字节索引 `&incoming.text[..incoming.text.len().min(80)]` 截取日志预览，当文本含中文（每字3字节）且超过约26个汉字时，80字节边界落在汉字中间，导致 `byte index N is not a char boundary` panic。

**修复**：改用字符级切片：
```rust
// 改前
&incoming.text[..incoming.text.len().min(80)]

// 改后
let preview: String = incoming.text.chars().take(80).collect();
```

这样无论文本是 ASCII 还是多字节 UTF-8，都能安全截取前80个字符。

---

## HIGH-2: safe_split_point 未处理 Assistant+tool_calls 边界（compact.rs）

**问题**：`safe_split_point` 只向前退让 `Role::Tool` 消息，忽略了 `Role::Assistant && !tool_calls.is_empty()` 的情况。当分割点落在一个带 `tool_calls` 的 Assistant 消息上时，压缩后的 kept 段会以 `[System(summary), Assistant(tool_calls), Tool, ...]` 开头，违反 OpenAI/Anthropic 要求"第一条非 System 消息必须是 User"的规则，导致 API 返回 HTTP 400/422。

**修复**：扩展退让逻辑，同时跳过 Tool 消息和带 tool_calls 的 Assistant 消息，直到找到 User 或 System 消息：

```rust
loop {
    if split == 0 || split >= transcript.len() { break; }
    let msg = &transcript[split];
    let should_back_up = msg.role == Role::Tool
        || (msg.role == Role::Assistant && !msg.tool_calls.is_empty());
    if should_back_up { split -= 1; } else { break; }
}
```

**副作用与测试更新**：  
修复后，单轮对话（只有一条初始 User 消息）的分割点从 `split=N`（落在 Asst+tc）退让到 `split=1`（User 消息之前），意味着每次压缩只移除 System 提示词（1条消息），不会让 transcript 大幅缩短。因此 in-kernel 压缩后 transcript 仍超阈值，触发 cross-turn 压缩。集成测试 `hooks_and_compaction` 从 5 个 mock completion 增加到 6 个。

---

## MEDIUM-2: sanitize_for_refname session ID 碰撞（checkpoint.rs）

**问题**：`sanitize_for_refname` 将 `.` 替换为 `-`，导致 `sess.1` 和 `sess-1` 映射到相同的 git ref `refs/sessions/sess-1/HEAD`，第二个 session 的 checkpoint 静默覆盖第一个的历史。macOS 临时目录路径（`.tmpXXX`）真实存在 `.`，碰撞非理论风险。

**修复**：对 `.` 和 `-` 使用不同的编码：
```rust
// 改前
sid.replace('.', "-")

// 改后
sid.replace('.', "_dot_").replace('-', "_dash_")
```

同步更新了测试 `sanitize_for_refname_collapses_dots` → `sanitize_for_refname_no_collision`，验证 `sess.1` 和 `sess-1` 映射到不同 git ref。

---

## Notes

- LOW-1（JoinHandle 泄漏）和 MEDIUM-3（重复 split 计算）本轮未修复，风险较低且需要更大范围重构。
- HIGH-2 的修复让压缩更保守（每次只压缩 System 消息），对于有中间 User 消息（多轮对话）的场景仍可正常压缩大量消息。
- 所有质量门全部通过：`cargo fmt --all` / `cargo clippy --all-targets --all-features -- -D warnings` / `cargo test --workspace`。
