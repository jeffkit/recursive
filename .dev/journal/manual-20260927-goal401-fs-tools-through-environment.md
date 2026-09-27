# Journal — Goal 401: Read / Write / Edit 走执行环境（transport 接电）

- **Date**: 2026-09-27
- **Goal**: [401-route-fs-tools-through-environment](../../goals/401-route-fs-tools-through-environment.md)（issue #28）
- **Branch**: `feat/goal-401-transport-fs`（worktree `.worktrees/goal-401-transport-fs`）
- **Base**: `c359a30` — Goal 400 的产物（issue #27，由 #29 的处理 run 先落，尚未合入 main；
  本分支直接以该 commit 为基，保证 400 只有一份，不会重复落库）

## Files touched

| 文件 | 改动 |
|------|------|
| `src/tools/fs.rs` | `ReadFile` / `WriteFile` 增加 `transport: Arc<dyn ToolTransport>` 字段 + `with_transport`；`tokio::fs::read/write/read_to_string/create_dir_all` 全部替换为 transport 调用；新增 `is_retryable_transport_error` / `transport_io_error` / `read_via_transport_or_empty` 三个 pub(crate) 辅助函数；新增 8 个测试（含 `MemTransport` 测试替身） |
| `src/tools/edit.rs` | `EditTool` 同上注入；读当前内容、写回、空 old_string 创建路径全部走 transport；新增 4 个测试 |
| `src/tools/registry.rs` | `build_standard_tools_with_roots`：`let base = ToolRegistry::local(); let shared_transport = base.transport().clone();` 后注入 Read/Write/Edit —— registry 的 transport 与工具的 transport 是**同一个 Arc** |
| `crates/recursive-cli/src/cli/builder.rs` | 第二个生产构造点同样接电：`ToolRegistry::new(transport.clone())` + 三个工具 `.with_transport(transport.clone())`（单实例共享，注释写明禁止再造第二个） |

未触碰（遵守 Files NOT to touch）：`shell.rs` / `glob.rs` / `search.rs` / `count_lines.rs`（402）、
`permission_pipeline.rs`、`dispatch.rs`、`run_core.rs`、`kernel.rs`、`runtime.rs`、
`transport.rs`（trait 定义冻结，零改动）。

## Tests added（均按名可跑）

fs.rs：`read_file_uses_transport`、`write_file_uses_transport`、
`read_via_registry_hits_registry_shared_transport`、`sandbox_escape_rejected_before_transport`、
`read_file_transport_retryable_error_marked`、`read_file_transport_non_retryable_error_not_marked`、
`write_file_transport_retryable_error_marked`

edit.rs：`edit_uses_transport_for_read_and_write`、`edit_empty_old_string_creates_file_via_transport`、
`edit_transport_retryable_error_marked`、`edit_transport_invalid_utf8_message_preserved`

关键证明：`MemTransport` 替身上 Read/Write/Edit 全部成功且**宿主文件系统上不存在该路径**
（`virt.exists()` == false）—— 证明 I/O 没有走 `tokio::fs`；
`sandbox_escape_rejected_before_transport` 断言逃逸路径被 `resolve_within_any` 拒绝时
transport 的 `read_calls == 0`（invariant #3：路径校验先于 transport）。

## 行为不变证明（LocalTransport 下）

- 改动前：`cargo test --lib` **2276 passed / 0 failed**（其中 `tools::fs` + `tools::edit` 87 个）。
- 改动后：`cargo test --lib` **2287 passed / 0 failed**（同 87 个既有 fs/edit 测试 + 11 个新增，零修改、零删除）。
- `cargo test --workspace`：3479 passed / 0 failed（与基线一致，新增即上述 11 个）。
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`：绿。
- `cargo fmt --all`：已跑。
- 沙箱 invariant：**注意目标名是 `--test invariants`**（`tests/invariants/sandbox.rs` 是
  `invariants` target 内的 `sandbox` 模块，不存在字面的 `--test sandbox` target）：
  39 passed / 0 failed，含 `readfile/writefile/edit_rejects_escape`。
- 新测试按名跑：11/11 通过（见上方清单）。

## 失败分类的落法（与 issue Scope 3 的偏差说明）

issue 预期「若 transport 返回 `TransportFailure::Retryable`」则标注。核实 Goal 400 产物
（c359a30）后发现：`TransportFailure` 只挂在 `ExecResult`（`exec_shell` 路径）上，
**fs 方法的 trait 签名仍是 `std::io::Result`**（trait 已冻结，本 goal 不扩）。
因此 fs 侧的分类实现为：`is_retryable_transport_error` 把瞬时 `io::ErrorKind`
（`TimedOut` / `WouldBlock` / `Interrupted` / `Connection*` / `BrokenPipe` /
`NotConnected` / `UnexpectedEof`）映射到 400 的 Retryable 语义，命中时错误文本加
`retryable: …(transient transport failure — retrying may succeed)` 前缀，测试 pin 了
正反两面（TimedOut/ConnectionReset 标注、NotFound 不标注）。未实现自动重试（按 scope）。
真正的 fs 级 `TransportFailure` 通道需要 trait 演进，列为后续候选。

## 已知放大点与残留（按 issue Notes 记录）

1. **大文件全量传输**：`read_file` 返回全量字节（trait 无 `stat`），远端档下读 100 MB
   文件会先传输再截断。本 goal 保持行为不变；「按需 range read / stat 方法」列为后续
   goal 候选。
2. **宿主侧残留**（均有注释说明，属「确实需要」类）：
   - `Edit` 的 `tokio::fs::metadata`（MAX_EDIT_FILE_SIZE 尺寸守卫）：trait 无 metadata；
     远端档下退化为「守卫静默跳过」（宿主 NotFound 走新文件豁免），**不会误拒**。
   - `Write` 的 `abs.exists()` 与全部 `get_file_mtime`（`std::fs::metadata`）：
     `ReadFileState` 的 mtime 语义本 goal 冻结，trait 无 stat。容器档下 mtime 校验
     需要随 trait 演进补齐（候选：`stat`/`exists` 进 trait，或改用内容哈希）。
   - 无 tmp 文件残留：`rg "tokio::fs::(read|write)" src/tools/fs.rs src/tools/edit.rs`
     仅剩注释中的引用。
3. **Edit 写回原子性**（issue 末尾 trap）：本地 Edit 之前就是 `tokio::fs::write` 直写
   （非 tmp+rename），本 goal 换成 `transport.write_file` 后语义不变——
   `LocalTransport.write_file` = `create_dir_all(parent)` + `tokio::fs::write`，
   不引入环境外临时文件，**无需新 trait 方法**。
4. **WriteFile 父目录行为**：之前是显式 `create_dir_all` + `tokio::fs::write`（不建父目录，
   靠显式 mkdir）；现在 transport 约定 `write_file` 自建父目录，显式 mkdir 仍保留（走
   transport，错误文本改为经 `transport_io_error`）——`write_creates_parent_dirs` 等
   既有测试全绿。

## 协同记录（与 #29 并行的冲突面）

`goal-402-transport-walk` worktree 正在跑 #29（Glob/Grep/count_lines 接电），与本 goal
在两个文件上有**预期中的行级冲突**（里程碑 57 行的「文件集不重叠」假设过于乐观——两边都要
改同一个 `build_standard_tools_with_roots` 和 CLI builder）：

- `src/tools/registry.rs`：402 侧给 SearchFiles/GlobTool 加 `.with_transport(shared_transport.clone())`，
  本侧给 Read/Write/Edit 加。**合并取并集即可**，preamble
  （`let base = ...; let shared_transport = base.transport().clone();`）两边写法一致。
- `crates/recursive-cli/src/cli/builder.rs`：同上，`let shared_transport = transport.clone()`
  的共享 preamble，两侧各自给各自工具加 `.with_transport(transport.clone())`。

后合并的一方照此取并集，属机械解冲突。本分支基于 c359a30（与 #29 分支共享 400 的同一个
commit sha），不会造成 400 重复落库。

## 备注

- GitNexus MCP 工具在本 session 不可用（无 `mcp__gitnexus__*` 工具面），按 CLAUDE.md 要求
  以 rg 手工完成了 impact 分析：`ReadFile`/`WriteFile`/`EditTool` 的全部构造点（registry.rs、
  cli/builder.rs、各测试）已枚举并全部兼容（字段带默认值、builder 可选）；
  `build_standard_tools_with_roots` 的两个调用方（TUI runtime_builder ×2）签名未变。
- 本 goal 的「工具 transport == registry transport == 环境绑定 transport」在
  `build_standard_tools_with_roots` 与 CLI builder 两处均成立（同一个 Arc 的 clone）；
  该函数目前硬编码 local 档，`Arc::new(LocalTransport)` vs `base.transport().clone()` 的
  变异要等 Goal 403 参数化档位后才可测——已用 CLI builder 路径的
  `read_via_registry_hits_registry_shared_transport` 钉住注入模式的语义。
