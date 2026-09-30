# 02 — acp feature gate（#54）改动方案（待 #53 落地后执行）

> 架构缺陷系列 2/9 · P1 · 依赖 #53（截至 2026-09-30 仍 OPEN，已重新入队按依赖顺序派发）。
> 本文件为 #54 的执行清单；事实核对于 recursive main @ `5a79cd6`。

## 现状核对（条目内证据全部属实）

| 事实 | 位置 | 核对结果 |
|---|---|---|
| `pub mod acp;` 无门禁 | `src/lib.rs:20` | ✔（`mcp`/`http` 均有 `#[cfg(feature = ...)]`） |
| 依赖非 optional | `Cargo.toml:101` `agent-client-protocol-schema = "1.4"` | ✔ |
| `src/acp/` 体积 | 7 文件（bridge/mod/permission/protocol/server/session/tool_kind）共 6,953 行 | ✔ |
| default features | `Cargo.toml:29`，无 `acp`（因 acp 目前无条件编译） | ✔ |
| `Cmd::Acp` 无门禁 | `crates/recursive-cli/src/main.rs:250`（variant）/ `:709-722`（handler） | ✔；仓内先例：`Cmd::Http` variant+arm 双 `#[cfg(feature = "http")]` |
| 工具层硬依赖 | 8 个工具文件 `use crate::acp::ToolKind`（registry/edit/shell/fs/glob/web_fetch/web_search/client_fs） | ✔ → 由 #53 解锁 |
| recursive-tui | 对 `recursive::acp` 零引用 | ✔ 无需加 `acp`，落地后自动获益 |
| recursive-cli 显式 features | `["cli","mcp","web_fetch","web_search","anthropic","http","skill-hub"]` | 需同步加 `acp` |
| recursive-tui 显式 features | `["mcp","web_fetch","web_search","anthropic","http","skill-hub"]` | 不加（无引用） |
| 构建入口显式 features | `Dockerfile:59`（`--features http`，走 cli 依赖行）；`.dev/scripts/agent-mutants.sh:38`（`FEATURES="test-utils,anthropic,http,mcp,web_fetch,web_search,skill-hub,coordinator-mode"`） | 均在同步清单内 |

## 改动清单

1. 根 `Cargo.toml`
   - `agent-client-protocol-schema = { version = "1.4", optional = true }`
   - `[features]` 增 `acp = ["dep:agent-client-protocol-schema"]`
   - `default` 增 `"acp"`（保留默认：本条价值在「可以关掉」，不在「默认关掉」）
2. `src/lib.rs:20` → `#[cfg(feature = "acp")] pub mod acp;`
3. `crates/recursive-cli`
   - `Cargo.toml`：recursive 依赖显式 features 增 `"acp"`（防静默丢失）；
     `[features]` 增转发项 `acp = ["recursive/acp"]`（照 web_search/http 既有注释模式）
   - `main.rs`：`Cmd::Acp` variant 与 match arm 加 `#[cfg(feature = "acp")]`（照 `Cmd::Http` 先例）
4. `crates/recursive-tui`：不改（零引用）；落地后 `cargo check -p recursive-tui` 验证
5. 守卫与配套
   - CI 增两组合：`cargo check --no-default-features --lib`、
     `cargo check --no-default-features --features http --lib`
   - `Dockerfile:59` 零改动即保持 ACP（cli 依赖行已含 `acp` 后自动跟随）；
     镜像内验证一次 `recursive acp --help`
   - `.dev/scripts/agent-mutants.sh:38` FEATURES 加 `acp`（保持现有突变覆盖）
   - `.dev/scripts/cli-mutants.sh` FEATURES 为空（吃 cli default/依赖行），自动跟随

## 验收（照 #54 Acceptance）

- `cargo check --no-default-features --lib` 通过（不开 `acp`）
- `cargo check --no-default-features --features http --lib` 通过
- `cargo build` 默认行为不变，ACP 用户无感知
- `recursive acp` 子命令仍可用（cli 已显式开启 `acp`）
- CI 覆盖「无 acp」组合
- 三门齐过：`cargo test --workspace` / `cargo clippy --all-targets --all-features -- -D warnings` / `cargo fmt --all`

## 执行前提

#53 合入后，按本清单在 `.worktrees/` 专属分支执行；纯机械改动，预计一个工作单元。
