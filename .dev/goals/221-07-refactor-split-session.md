# Goal 221 — Refactor: 拆分 `session.rs` (2214 → 5 文件)

**Roadmap**: Phase 20 — 0.7 Refactor & Hardening
**依赖**: Goal 219（同理，先清旧路径）
**类型**: A — 架构级重构（人/Claude 主导）

## Why

`src/session.rs` 2214 行，被 agent / runtime / tools / cli / tui / tests 几乎**所有**模块 import。`grep -c "^impl"` 显示 10 个 `impl` 块——职责高度混杂：transcript 持久化、UUID 链、orphan tool-call 配对、session 生命周期、配额、checkpoint 协调全部在同一个文件里。

这是 `Goal 219 之后`的第二个 god module。

## Design

### 拆分目标

```
src/session/
  mod.rs                  ← pub use 出口 + 公共类型（≤ 150 行）
  serialize.rs            ← TranscriptEntry / entry_to_message 编码（≤ 400 行）
  lifecycle.rs            ← SessionLock、open/close、truncate_transcript_to_turn（≤ 400 行）
  orphan.rs               ← OrphanToolCall 检测与处理（≤ 400 行）
  reader.rs               ← SessionReader（流式读，已存在，迁入）（≤ 500 行）
  writer.rs               ← SessionWriter、SessionPersistenceSink（已存在，迁入）（≤ 500 行）
```

### 公开 API 保持不变

`lib.rs:100-108` 的 8 个 `pub use` 全部保留为从 `session::` 重导出。下游 `use crate::session::X` 一行不动。

### 现有结构盘点（基于 lib.rs re-export）

| 公开类型 | 当前文件 | 目标 |
|---|---|---|
| `OrphanToolCall` | `session.rs` | `orphan.rs` |
| `SessionFile` | `session.rs` | `mod.rs`（re-export） |
| `SessionPersistenceSink` | `session.rs` | `writer.rs` |
| `SessionReader` | `session.rs` | `reader.rs` |
| `SessionWriter` | `session.rs` | `writer.rs` |
| `SessionLock` | `session_lock.rs` | `lifecycle.rs`（合并 `session_lock.rs`） |
| `SessionMeta` | `session.rs` | `mod.rs`（re-export） |
| `TranscriptEntry` | `session.rs` | `serialize.rs` |
| `TruncateStats` | `session.rs` | `lifecycle.rs` |
| `entry_to_message` | `session.rs` | `serialize.rs` |
| `truncate_transcript_to_turn` | `session.rs` | `lifecycle.rs` |

### 合并 `session_lock.rs`

`session_lock.rs` 295 行是 session 的文件锁子模块，应作为 `lifecycle.rs` 的一部分，而不是独立文件。删除 `src/session_lock.rs`，代码迁入 `session/lifecycle.rs`，`lib.rs:42` 的 `pub mod session_lock;` 改为 `pub mod session;`（因为 `session_lock` 是 session 的子模块，不再平级）。

## 验收标准

- `src/session.rs` ≤ 10 行（仅 `pub mod serialize; pub mod lifecycle; ...`），最终应改为 `src/session/mod.rs`
- `src/session_lock.rs` 已删除（合并入 `lifecycle.rs`）
- 6 个新文件总和 ≈ 原 2214 + 295 = 2509 行 ± 150 行
- `grep -rn "use crate::session_lock" src/ tests/` 返回 0
- `cargo test --workspace` 全绿
- `cargo clippy --all-targets --all-features -- -D warnings` 干净

## Non-goals

- 不改任何 `pub use crate::session::X` 路径
- 不重命名 `SessionLock` 类型
- 不改文件锁的协议或行为
