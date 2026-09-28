# Goal 394 — 会话级工具状态隔离（`fork()` 不再是 `clone()`）

**Roadmap**: Phase 17 Production Hardening + 多会话宿主前置。
这是**正确性 bug**，不只是性能：HTTP 上两个互不相关的会话今天共享 read-before-edit 护栏、
touched-files 与后台任务管理器。

**依赖**: 无。

**Design principle check**:
- Implemented as: 让 `ToolRegistry` 提供一个**真正重建会话级可变状态**的 fork 路径，
  并把 HTTP 的会话创建改用它；共享项（transport / permissions / MCP client）保持共享并写清理由。
- ❌ Does NOT 改 `Tool` trait 的 `execute` 签名（会牵动全部工具；构造期注入是既定方式）。
- ❌ Does NOT 改 `resolve_within` 路径校验（invariant #3）。
- ❌ Does NOT 改工作区级共享语义（Scratchpad / memory / fact store 属于工作区，不是会话）。

## Why（2026-09-27 核实）

```rust
// src/tools/registry.rs:104-111
/// NOTE: Clone shares Arc state with all tools. Use fork() for isolation.
#[derive(Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    aliases: BTreeMap<String, String>,
    transport: Arc<dyn super::transport::ToolTransport>,
```

```rust
// src/tools/registry.rs:220-224
/// For now, this is equivalent to `clone()` — a full fork requires
/// per-tool fork support.
pub fn fork(&self) -> Self { self.clone() }
```

`Tool` trait 里**根本没有** `fork()` 方法（`src/tools/registry.rs:30-75`），所以这段注释
是一个不存在的承诺。`Clone` 的 derive 会把下列 `Arc` 原样共享：

- `read_file_state: Option<Arc<Mutex<ReadFileState>>>`（`:123`，最多缓存 100 个文件正文、
  单文件上限 256 KB → 进程级最多 ~25.6 MB，且是 read-before-edit 护栏的唯一凭据）
- `touched`: `Arc<Mutex<TouchedFiles>>`（`:120`）
- `bg_manager`：后台任务管理器（`build_standard_tools_with_roots` 内 `:683-687` 构造）
- `session_roots: Option<SharedSandboxRoots>`（`:127`）——CLI 在
  `crates/recursive-cli/src/cli/builder.rs:62` 用 `new_shared_sandbox_roots()` 建一次；
  HTTP 直接 `state.tool_registry.clone()`（`src/http/handlers.rs:272`）→ **一个会话的
  `--add-dir` 式扩容会影响所有会话**

HTTP 上每个会话都走 `clone()`，于是：

- 会话 A 读过的文件会让会话 B 的 `Edit` 通过「已读校验」——护栏失效（正确性）。
- `touched_files` 混在一起：checkpoint / 审计归属错误。
- 后台任务（`run_background`）跨会话可见。

这也解释了实测中「空闲会话 ~50 KB」的构成：其中 6–8 KB 是 per-session 复制的
BTreeMap 节点，而真正的会话态却是共享的——**成本付了，隔离没拿到**。

## Scope（do exactly this, no more）

### 1. 提供真正的会话 fork

在 `src/tools/registry.rs` 增加（命名二选一，倾向第一个）：

```rust
/// 重建全部会话级可变状态；共享传输/权限/MCP 等外部资源。
pub fn fork_session(&self) -> Self;
```

要求：

- 重建：`read_file_state`（`ReadFileState::new()`）、`touched`（`TouchedFiles::new()`）、
  `bg_manager`（`BackgroundJobManager::new()`）、`session_roots`（`new_shared_sandbox_roots()`）。
- 保持共享：`transport`、`permissions`、`permission_mode`、`permission_hook`、`policy`、
  `elicitation`、以及 `tools` 里的 `Arc<dyn Tool>` **对象本身**——但注意：
  `ReadFile` / `EditTool` / `WriteFile` 持有的是**构造期注入的 `read_state` Arc**，
  所以只替换 registry 字段**不够**，必须重新构造这些工具（见下条）。
- 因此 `fork_session` 需要能重新构造受影响的工具。最简做法：把
  `build_standard_tools_with_roots` 里「会话级状态的构造」抽成一个
  `SessionToolState { read_state, touched, bg_manager, session_roots }`，
  并让 `fork_session` 用新 state 重新走一遍受影响工具的注册（把受影响的工具名单
  写成常量列表，避免猜测）。
- 明确保留 `fork()`（已存在，语义仍是 clone）以免破坏现有调用点；对
  `fork()` 的文档注释做出**诚实修正**：说明它不做隔离、指向 `fork_session()`。
  若 grep 显示 `fork()` 只有 HTTP/多代理的调用点，优先**直接把那些点改为
  `fork_session()`**，并让 `fork()` 变成 `fork_session()` 的别名（更少概念）。

### 2. HTTP 会话创建改用 fork

- `src/http/handlers.rs:272`（create_session）与 `:151`、`:593`、`:1663` 等 clone 点改为
  `fork_session()`；`fork_session` 的 HTTP endpoint（会话分叉）也要用新的隔离语义。

### 3. 测试（必须 pin 行为，agent-mutants 门）

- 两个 `fork_session()` 出来的 registry：会话 A `Read` 同一文件后，会话 B 的 `Edit`
  **不得**因 A 的读而通过（护栏按会话独立）。
- `touched_files`：A 的写入不出现在 B 的 touched 集合里。
- `bg_manager`：A 注册的后台任务在 B 不可见。
- `session_roots`：A 扩容的 root 不影响 B。
- 同一会话内：`Read` → `Edit` 的护栏**仍然生效**（防止修过头把功能改坏）。

## Files NOT to touch

- `src/run_core.rs`、`src/kernel.rs`、`src/runtime.rs`（内核与 line-budget）。
- `src/tools/dispatch.rs` 的 `resolve_within` / 权限流水线语义（invariant #3）。
- Scratchpad / memory / facts 的共享语义（它们是工作区级，另有设计）。
- `src/mcp.rs` 的 client 共享策略（MCP 是外部资源，**有意**按服务器共享；
  在 `fork_session` 的文档里写清这一条，避免后人误当 bug 修）。

## Acceptance

- `cargo test --workspace`、`cargo clippy --workspace --all-targets --all-features -- -D warnings`、
  `cargo fmt --all` 全绿。
- `cargo test --test sandbox`（`tests/invariants/sandbox.rs`）绿。
- 新测试按名可跑：`cargo test --lib fork_session`。
- Grep: `rg "fork_session" src/ | wc -l` ≥ 5（定义 + 文档 + registry 测试 + HTTP 调用 + HTTP 测试）。
- `rg "fn fork\(" -A 5 src/tools/registry.rs` 的注释不再声称 fork 提供隔离
  （或已被 `fork_session` 取代）。
- Journal: `.dev/journal/manual-20260927-goal394-session-tool-isolation.md`，
  列出「重建 vs 共享」的完整清单及理由。

## Notes for the agent (traps)

- **只改 registry 字段是不够的**：`ReadFile`/`EditTool`/`WriteFile` 在构造期拿到了
  `Arc<Mutex<ReadFileState>>`，必须随 fork 重新构造这些工具实例——这是本 goal 最容易
  漏掉的点，先 `rg "read_file_state" src/tools/` 把所有持有者列全再动手。
- **不要改 `Tool::execute` 签名**去传上下文；本 milestone 的方向是构造期注入
  （Goal 400 定义契约、Goal 401/402 沿这条路接电），现在改签名会让它们无从下手。
- **HTTP 的 `state.tool_registry` 是启动时构建的模板**：`fork_session()` 必须能在
  不重新做 MCP 连接、不重新扫描 skills 的前提下工作（MCP/skills 属于启动期资源）。
- 保持 `with_read_file_state` / `read_file_state()` 现有语义与测试
  （`src/tools/registry.rs:1215-1225`），它们被 CLI 的 reinjector 用到。

