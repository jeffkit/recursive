# Goal 214 — 拆分 src/main.rs：按职责提取 CLI 子模块

**Roadmap**: 代码健康 — 大文件专项整治（第三批）

**设计原则检查**:
- 纯代码组织重构，运行时行为不变
- 新建 `src/cli/` 目录，将函数迁移到子模块
- 所有函数保持原签名，通过 `use` 引入即可，无需 pub re-export（main.rs 内部函数）
- ❌ 不改变命令行接口（CLI flags/subcommands 不变）

## 背景

`src/main.rs` 当前 **3140 行**，远超单文件上限。它同时承担了：
CLI 参数解析、运行时构建、session 管理、resume 逻辑、输出格式化、
MCP 注册、以及 5 种不同的执行模式（run / loop / repl / resume / init）。

## 目标

新建 `src/cli/` 目录，将 `main.rs` 中的非核心函数迁移到 4 个子模块：

| 新文件 | 迁移内容 | 预估行数 |
|--------|---------|---------|
| `src/cli/builder.rs` | `build_tools`, `build_runtime`, `register_mcp_tools`, `register_mcp_server_tools`, `discover_loaded_skills`, `resolve_tool_permissions` | ~550 |
| `src/cli/output.rs` | `get_pricing`, `print_usage`, `print_finish_note`, `save_transcript`, `save_session`, `exit_for_finish`, `finalize_session_writer`, `finalize_cost_tracker`, `stream_events`, `stream_events_repl`, `stream_events_json` | ~400 |
| `src/cli/resume.rs` | `cmd_resume`, `run_resumed`, `resolve_resume_target`, `legacy_resume_error`, `prompt_orphan_choice`, `OrphanPolicy` enum | ~450 |
| `src/cli/session.rs` | `cmd_migrate`, `cmd_session_migrate_legacy`, `cmd_session_rewind`, `resolve_session_path` | ~250 |
| `src/main.rs` (保留) | `main()`, `run_once`, `run_loop`, `run_init`, `repl`, `shutdown_signal`, `mask_key`, `init_logging`, CLI struct/enum 定义, `run_mcp_server_stdio`, `dispatch_request_via_registry` | ~1500 |

拆分后 `main.rs` 预计从 3140 行 → ~1500 行。

## 实施细节

### 1. 新建目录和模块文件

```
src/cli/
  mod.rs
  builder.rs
  output.rs
  resume.rs
  session.rs
```

`src/cli/mod.rs` 内容：
```rust
pub(crate) mod builder;
pub(crate) mod output;
pub(crate) mod resume;
pub(crate) mod session;
```

### 2. 在 `src/lib.rs` 或 `src/main.rs` 声明 cli 模块

由于 `src/main.rs` 是二进制 crate 入口，不走 `lib.rs`，
直接在 `src/main.rs` 顶部加：

```rust
mod cli;
```

### 3. 迁移各函数

对每个子模块，将对应函数**原样剪切**到目标文件。
每个目标文件需要自己的 `use` 列表（从 main.rs 的 use 列表中取需要的）。

函数可见性：这些函数原本都是 `main.rs` 的私有函数，迁移后改为
`pub(crate)` 以便 `main.rs` 通过 `cli::builder::build_tools(...)` 调用。

### 4. 更新 `src/main.rs` 的调用

将所有对已迁移函数的直接调用改为 `cli::builder::build_tools(...)` 等形式，
或在 `main.rs` 顶部加 `use crate::cli::builder::*;` 通配引入（优先前者，更清晰）。

### 5. OrphanPolicy enum 迁移

`resume.rs` 中包含一个本地 enum `OrphanPolicy`，迁移时一并移入 `cli/resume.rs`。

## 验收标准

1. `cargo build --all-features` 通过
2. `cargo test --workspace` 全绿
3. `cargo clippy --all-targets --all-features -- -D warnings` 干净
4. `cargo fmt --all -- --check` 干净
5. `src/main.rs` 行数 **≤ 1600**
6. `src/cli/` 目录下 4 个子模块文件均存在

## 明确不在范围内

- ❌ 不改变 CLI 参数结构（`Cli`、`Commands` enum 保持在 main.rs）
- ❌ 不改变 `repl()` 的实现（保留在 main.rs，逻辑复杂）
- ❌ 不拆分 `run_once` 和 `run_loop`（保留在 main.rs）
- ❌ 不改变任何函数签名

## 注意事项

- `main.rs` 是二进制 crate（`src/main.rs` in Cargo.toml），声明子模块用 `mod cli;` 而不是 `pub mod cli;`
- 迁移函数时注意循环依赖：`builder.rs` 不能 use `output.rs` 里的东西
- `anyhow::Result` 在各子文件中需要 `use anyhow::Result;` 或写全路径
- 生命周期参数、复杂的泛型签名原样保留，不简化
