# Goal 406 — 失败分类提升到 trait 层：fs 方法不再裸返 `std::io::Result`

**Roadmap**: milestone 批次 3 追加（Goal 401 落地时发现的契约缺口，2026-09-28 立项）。

**依赖**: Goal 400（`TransportFailure` 分类与 `capabilities()`）、Goal 401（fs 接电与
`io::ErrorKind` 启发式的先例）；应在 Goal 403（容器档）合入**之前**完成——容器档恰恰是
最需要区分「环境故障 vs 代码问题」的档位。

**Design principle check**:
- Implemented as: **扩展现有 `ToolTransport` 契约**——把 `ExecResult.failure`
  （`src/tools/transport.rs`，Goal 400）的分类语义提升为 trait 级错误类型，不新建第二套错误体系。
- ❌ Does NOT 改变任何工具对模型暴露的 schema / 成功路径输出格式（只动错误分类）。
- ❌ Does NOT 在调用方继续散落启发式（现状：401 在调用方用 `io::ErrorKind` 猜重试性，
  每接一批新工具就要重猜一次）。

## Why（2026-09-28 核实）

trait 现状（`src/tools/transport.rs`，`ToolTransport`）：

| 方法 | 返回 | 失败分类 |
|---|---|---|
| `exec_shell` | `io::Result<ExecResult>` | ✅ `ExecResult.failure: Option<TransportFailure>`（`Retryable` / `Environment` / `Tool`） |
| `read_file` / `write_file` / `list_dir` / `create_dir_all` | `std::io::Result` | ❌ 无分类 |

后果：本地档下 `io::ErrorKind` 够用；容器档下同一个 `NotFound` 可能是「容器没起来 /
卷没挂上」（环境故障，可重试）也可能是「模型写错路径」（代码问题，不可重试）——
重试器、finish_reason、观测层都无法区分，只能猜。401 已用启发式过渡，但启发式长在调用方。

## Scope（do exactly this, no more）

### 1. trait 级错误类型
- `src/tools/transport.rs`：新增 trait 级错误（例如
  `TransportError { failure: TransportFailure, source: io::Error }` 或等价形状），
  四个 fs 方法签名改为返回它；`TransportFailure` **复用** Goal 400 的三个变体，不新造类别。
- `LocalTransport`：`io::ErrorKind → TransportFailure` 的映射收敛为**一处**实现
  （把 401 散在调用方的启发式搬进来，调用方删掉）。

### 2. 调用方迁移
- 401/402 已接电的工具（`fs.rs` / `edit.rs` / `glob.rs` / `search.rs` / `count_lines.rs`）
  改为消费分类结果；重试标注不再自行推断，错误文本格式保持 401 的既有约定。
- Goal 403 的容器实现届时可直接表达「环境未就绪」，不需要 exec 之外的第二个通道。

### 3. 测试（agent-presence / agent-mutants 门）
- 假 transport 验证：`Retryable` / `Environment` 分类能穿透到工具错误信息
  （与 401 的标注格式兼容）。
- 映射表单测：`io::ErrorKind` → 分类逐项 pin 住（`NotFound`/`PermissionDenied`/`TimedOut`/
  `ConnectionRefused` 等，写全用过的分支）。
- 本地档回归：现有 fs / edit / glob / search 测试全绿。

## Files NOT to touch

- `src/run_core.rs`、`src/kernel.rs`、`src/runtime.rs`（invariant #1）。
- `src/tools/dispatch.rs` 的路径校验与审计语义（invariant #3）。
- 任何工具的 schema 与成功路径输出（e2e replay fixture 依赖）。

## Acceptance

- `cargo test --workspace`、`cargo clippy --all-targets --all-features -- -D warnings`、
  `cargo fmt --all` 全绿。
- `ToolTransport` 的四个 fs 方法签名不再裸返 `std::io::Result`
  （`rg "async fn (read_file|write_file|list_dir|create_dir_all)" src/tools/transport.rs` 可验）。
- Journal：`.dev/journal/manual-<date>-goal406-transport-failure-classification.md`，
  记录映射表与迁移的工具清单。

## Notes for the agent (traps)

- **不要借机重命名 / 移动 `TransportFailure`**：Goal 400 刚落地，401/402 已引用。
- **错误信息文本向后兼容**：模型 prompt 与 e2e 断言已见过 401 的标注格式，别让它漂移；
  要改格式先开独立 goal。
- **`walk`（Goal 402 新增）也在迁移范围**：它是 fs 面（返回 `io::Result<Vec<WalkEntry>>`），
  别只迁 401 的四个老方法漏掉它。
