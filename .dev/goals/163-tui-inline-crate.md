# Goal 163 — TUI: 内联到主 crate，成为默认交互入口

**Roadmap**: Phase 11 — TUI 架构对齐

**Design principle check**:
- 将 `crates/recursive-tui` 迁移为主 crate 的 `src/tui/` 模块
- 主二进制 `recursive` 无参数时默认启动 TUI，`-p` 保持 CLI 单次运行
- 删除 `crates/recursive-tui` 独立 crate

## Why

`crates/recursive-tui` 从未发布到 crates.io，只是工程上拆出的独立 crate。
这带来了不必要的复杂度：两个二进制、跨 crate 的类型导出、`build_standard_tools`
需要 pub 暴露等。

目标是让 `recursive` 命令的行为对齐 Claude Code：
- `recursive`（无参数）→ 打开 TUI 交互界面
- `recursive -p "goal"` → 单次非交互运行（CLI 模式）
- `recursive run/loop/config/...` → 现有子命令不变

## Scope (do exactly this, no more)

### 1. 迁移 TUI 源码

将 `crates/recursive-tui/src/` 下所有文件迁移到 `src/tui/`：

```
src/tui/
  mod.rs          ← 原 lib.rs 内容
  app.rs
  backend.rs
  bash.rs
  commands.rs
  events.rs
  keymap.rs
  runtime_builder.rs
  ui/
    mod.rs
    app.rs        ← 各 ui 子模块
    ...
```

### 2. feature flag

在主 `Cargo.toml` 的 `[features]` 中新增：

```toml
tui = ["dep:ratatui", "dep:crossterm", "dep:unicode-width"]
```

将 ratatui / crossterm / unicode-width 从 `always` 依赖改为 `optional`。
`default` features 中加入 `tui`（默认开启，用户可 `--no-default-features` 关闭）。

### 3. 在 `src/lib.rs` 中声明模块

```rust
#[cfg(feature = "tui")]
pub mod tui;
```

### 4. 修改 `src/main.rs` 的默认行为

当前逻辑（`main.rs:355-364`）：
```
无参数 → Cmd::Repl
-p "goal" → Cmd::Run { goal }
```

改为：
```
无参数 → 启动 TUI（调用 tui::run()）
-p "goal" → Cmd::Run { goal }（保持不变）
```

新增 `tui::run()` 函数作为 TUI 入口，内容对应原
`crates/recursive-tui/src/main.rs` 的 main 函数体。

`build_standard_tools` 不再需要 pub 到 lib 顶层（TUI 和 CLI 在同一 crate
内，直接用 `crate::tools::build_standard_tools`）——但保留 pub 导出也无妨，
不作为本 goal 的必须项。

### 5. 删除 `crates/recursive-tui`

- 从 workspace `Cargo.toml` 的 `members` 中移除 `"crates/recursive-tui"`
- 删除 `crates/recursive-tui/` 目录

### 6. 迁移测试

原 `crates/recursive-tui/tests/` 下的集成测试迁移到 `tests/tui_*.rs`
（workspace 级别的集成测试目录），或直接内联到 `src/tui/` 各模块的 `#[cfg(test)]`。

## Acceptance

1. `cargo build` 成功（默认 features，含 tui）
2. `cargo build --no-default-features --features cli,mcp,web_fetch,anthropic,http` 成功（不含 tui）
3. `cargo test --workspace` 全绿
4. `cargo run -- ` 无参数启动 TUI（不崩溃进入界面）
5. `cargo run -- -p "list files"` 走 CLI 单次运行（不启动 TUI）
6. `crates/recursive-tui` 目录不再存在
7. `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean

## Notes for the agent

- 迁移时注意 `crates/recursive-tui/src/backend.rs` 里的
  `use recursive::...` 全部改为 `use crate::...`（现在在同一 crate 内）
- `runtime_builder.rs` 里的 `recursive::build_standard_tools` 改为
  `crate::tools::build_standard_tools`
- 原 `crates/recursive-tui/Cargo.toml` 的依赖（ratatui / crossterm /
  unicode-width）合并到主 `Cargo.toml`，标记为 `optional`
- `crates/recursive-tui/tests/backend_smoke.rs` 的集成测试依赖
  `recursive_tui` crate，迁移后改为 `recursive` crate 内的测试，
  注意 feature gate（`#[cfg(feature = "tui")]`）
- TUI 的 `main()` 函数（`crates/recursive-tui/src/main.rs`）内容
  提取为 `src/tui/mod.rs` 中的 `pub fn run()` 函数
- 迁移完成后先跑 `cargo test --workspace` 确认全绿再删除旧 crate 目录
