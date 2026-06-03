# Goal 226 — Refactor: 抽出 `recursive-tui` 子 crate

**Roadmap**: Phase 20 — 0.7 Refactor & Hardening
**依赖**: Goal 220（先拆完 app.rs，再做 crate 化），以及 Goal 225
**类型**: A→B（设计阶段 A 主导 + 抽出阶段 B 由 loop 跑）

## Why

当前所有 TUI 代码（`src/tui/`，约 5000+ 行）坐在 main binary crate 里，每次 `cargo build` 都全量编译 TUI 依赖（ratatui、crossterm、syntect、pulldown-cmark），即使只跑 `recursive sessions rewind` 这种纯 CLI 命令。

子 crate 化的好处：
- 隔离编译：CLI 命令不依赖 ratatui → 冷启动更快
- 复用：未来如果有第二个 TUI 客户端（IDE 扩展、远程 web TUI），直接引用 `recursive-tui` 即可
- 与现有 `crates/agui-tui`、`crates/agui-protocol`、`crates/agui-client` 的模式对齐

## Design

### 子 crate 结构

```
crates/
  recursive-tui/                  ← 新建
    Cargo.toml                    ← name = "recursive-tui"
    src/
      lib.rs                      ← 公开 API
      app/                        ← 来自 src/tui/app/*（Goal 220 拆分后）
      backend.rs                  ← 来自 src/tui/backend.rs
      commands.rs
      events.rs
      input_state.rs
      keymap.rs
      model.rs
      runtime_builder.rs
      skill_commands.rs
      ui/                         ← 来自 src/tui/ui/*
  
src/tui/                          ← 退化为 re-export shell
  mod.rs                          ← pub use recursive_tui::*;
```

### main.rs 改动

`use crate::tui::*` → `use recursive_tui::*`，或者保持 `use crate::tui::*` 通过 re-export 不破坏。

### Cargo.toml 改动

- workspace 成员加 `"crates/recursive-tui"`
- `src/Cargo.toml` 的 tui 相关 dep 移到 `crates/recursive-tui/Cargo.toml`
- `src/Cargo.toml` 加 `recursive-tui = { path = "../crates/recursive-tui", optional = true }`
- `tui` feature 现在变成 `dep:recursive-tui`

### 依赖

新 `crates/recursive-tui/Cargo.toml` 的 deps：
- ratatui / crossterm / unicode-width / syntect / pulldown-cmark（从 main 移过来）
- recursive-agent = { path = "../..", default-features = false }（仅需要的 feature）

## 验收标准

- `crates/recursive-tui/` 存在并能独立 `cargo build`
- `src/tui/` ≤ 30 行（仅 re-export）
- main binary 默认 build 不再链接 ratatui（可通过 `cargo tree -p recursive-agent | grep ratatui` 验证）
- `cargo test --workspace` 全绿（包括 recursive-tui 自己的 tests）
- `cargo build --no-default-features --features cli`（最小 build）通过
- `cargo clippy --all-targets --all-features -- -D warnings` 干净

## 风险

- TUI 里有 `use crate::tui::commands::*` 之类的循环引用——子 crate 化时需要重新审查
- `runtime_builder.rs` 里 TUI 与 AgentRuntime 的胶水代码，跨 crate 边界需要 `pub` 化一些原本 crate-private 的类型
- 第一次 build 会显著慢（crate graph 重组），CI 接受这一痛点
