# Goal 401 — 让 `Read` / `Write` / `Edit` 走执行环境（transport 接电第一批）

**Roadmap**: milestone 批次 3。执行面契约（Goal 400）之后的第一批真实接电：
文件读写从「宿主进程直读」变为「经执行环境」，为容器/microVM 档铺路。

**依赖**: Goal 400（capabilities + 失败分类 + 路径语义契约）。

**Design principle check**:
- Implemented as: 在 `build_standard_tools_with_roots`（`src/tools/registry.rs:672+`）里
  把 registry 已持有的 `Arc<dyn ToolTransport>` 注入 `ReadFile` / `WriteFile` / `EditTool`
  的构造函数；工具内部把 `tokio::fs` 调用替换为 transport 调用。
- ❌ Does NOT 改 `Tool::execute` 签名（构造期注入是既定方式）。
- ❌ Does NOT 改 `resolve_within` 的调用位置与语义（invariant #3：路径校验永远在
  transport 之前，且仍是第一条防线）。
- ❌ Does NOT 改 `ReadFileState` 的缓存/护栏语义（它由 Goal 394 保证会话级隔离）。

## Why（2026-09-27 核实）

- 接缝已存在但没接电：`ToolTransport`（`src/tools/transport.rs:50-77`）定义了
  `read_file` / `write_file` / `list_dir` / `create_dir_all`，`ToolRegistry` 持有
  `transport: Arc<dyn ToolTransport>`（`src/tools/registry.rs:111`），但
  `rg "transport" src/tools/fs.rs src/tools/edit.rs` = **0 命中**——三个工具今天直接用
  `tokio::fs` 操作宿主文件系统。
- 后果：即使 Goal 403 把 `Bash` 放进容器，`Read`/`Write`/`Edit` 仍然作用在**宿主**上，
  于是「沙箱」只沙箱了命令执行——模型写文件到宿主、跑命令在容器，行为不一致且更危险。
- 这是 milestone 里唯一能把「沙箱」从口号变成事实的一步，因此顺序上必须在容器档之前。

## Scope（do exactly this, no more）

### 1. 构造期注入 transport

- `ReadFile` / `WriteFile` / `EditTool` 增加 `Arc<dyn ToolTransport>` 字段
  （与既有 `root` / `read_state` 字段并列），由 `build_standard_tools_with_roots`
  在注册时注入 registry 的 transport。
- 构造顺序问题：`ToolRegistry::new(transport)`（`src/tools/registry.rs:159`）已经先有
  transport，构造工具时用 `registry.transport().clone()`（accessor 已存在，`:184-187`）；
  若 `build_standard_tools_with_roots` 是在 registry 之外先造工具再注册，则把
  `Arc<dyn ToolTransport>` 作为该函数的显式参数（选一种，不要两处各自 new 一个
  `LocalTransport`——那会让容器档失效）。
- **兼容性**：`LocalTransport` 仍是默认，因此现有行为不变（Acceptance 要求证明）。

### 2. 工具内部替换 I/O

- `ReadFile`：`tokio::fs::read` → `transport.read_file(path)`；`max_bytes` 截断、
  行区间切片、CRLF 归一化、`ReadFileState::record` 顺序**保持不变**
  （顺序错了会破坏 read-before-edit 的 mtime 校验）。
- `WriteFile`：`tokio::fs::write` → `transport.write_file(path, bytes)`
  （trait 约定「创建父目录」，`src/tools/transport.rs:59-61`）。
- `EditTool`：读当前内容仍走 transport；写回走 `write_file`；`ReadFileState` 的
  更新与 mtime 语义保持。
- `resolve_within_any` / `resolve_within` 调用位置不动（`src/tools/fs.rs` 现有路径解析逻辑
  先执行，再进行 transport 调用）。

### 3. 失败分类的消费（第一批）

- 若 transport 返回 `TransportFailure::Retryable`（Goal 400 引入的分类），工具错误信息
  必须**明确标注可重试**（例如 prefix `retryable: ...`），让模型知道这不是代码问题。
  具体文案自定，但必须在测试里 pin（mutants 门会检查）。
- 不要在这一步实现自动重试（重试策略属于 transport 实现或上层，本 goal 只做**语义标注**）。

### 4. 测试（agent-presence / agent-mutants 门）

- **假 transport 证明接电**：构造一个内存 transport（只存在于测试里），在其上放一个文件；
  断言 `Read` 能读到该内容，且**宿主文件系统上不存在该路径**（证明没有走 `tokio::fs`）。
  同理 `Write` 写进假 transport 后宿主无文件。
- `Edit` 的 read-before-edit 护栏：未读先改 → 拒绝；读后改 → 成功；改后文件内容与
  `ReadFileState` 一致（既有测试应继续绿）。
- 超大文件截断（`max_bytes`）与行区间读取行为不变（沿用既有测试，必要时补齐）。
- `cargo test --test sandbox`（`tests/invariants/sandbox.rs`）绿：
  路径逃逸必须在 transport 之前被拒绝。
- 失败分类标注：假的 `Retryable` transport → 工具返回的错误文本含重试语义标记。

## Files NOT to touch

- `src/tools/shell.rs`、`src/tools/glob.rs`、`src/tools/search.rs`、`src/tools/count_lines.rs`
  （属于 Goal 402）。
- `src/tools/registry.rs` 的权限流水线（`src/tools/permission_pipeline.rs`）与
  `dispatch.rs` 的审计字段。
- `src/run_core.rs`、`src/kernel.rs`、`src/runtime.rs`。
- `src/tools/transport.rs` 的 trait 定义（Goal 400 冻结；若确实缺方法，先改 400 的产物
  并在 journal 说明，不要在本 goal 私加 trait 方法）。

## Acceptance

- `cargo test --workspace`、`cargo clippy --workspace --all-targets --all-features -- -D warnings`、
  `cargo fmt --all` 全绿。
- `cargo test --test sandbox` 绿。
- 新测试按名可跑：`cargo test --lib read_file_uses_transport` 等（journal 记录确切测试名）。
- **行为不变证明**：`LocalTransport` 下 `cargo test --test smoke` 与既有 fs 相关单测全绿；
  journal 里贴出改动前后相同的测试列表。
- Grep: `rg "transport" src/tools/fs.rs src/tools/edit.rs` 有命中（注入 + 调用 + 测试）。
- Grep: `rg "tokio::fs::(read|write)" src/tools/fs.rs src/tools/edit.rs` 显著减少
  （残留仅限确实需要宿主侧的操作，例如 tmp 文件——若有，必须在注释里说明为什么）。
- Journal: `.dev/journal/manual-20260927-goal401-fs-tools-through-environment.md`。

## Notes for the agent (traps)

- **不要两处各自构造 `LocalTransport`**：容器档要求「工具用的 transport == registry 的
  transport == 环境绑定用的 transport」。这是本 goal 最容易埋下的静默失效。
- **`ReadFileState` 的顺序语义**：`record()` 必须在成功读取之后、且在任何截断逻辑之前
  按既有顺序调用；mtime 校验依赖它。
- **大文件**：transport 的 `read_file` 返回全量字节（trait 无 `stat`），这意味着远端档
  下读取一个 100 MB 文件会先传输再截断。本 goal **保持行为不变**，在 journal 里记录
  这个已知放大点，并把「按需 range read」列为后续 goal 候选（不要在这里扩 trait）。
- **`Edit` 的写回如果有原子写语义**（本地可能是写临时文件 + rename），容器档下
  临时文件会落在环境内——确认 `write_file` 语义后再决定是否需要新的 trait 方法。

