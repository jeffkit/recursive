# Goal 149 — TUI: 复用 ~/.recursive/config.toml（CLI 配置）

**Roadmap**: TUI 改造系列的紧急 bugfix（Goal 143-148 的遗漏）

**Design principle check**:
- 仅改 `crates/recursive-tui/src/backend.rs::build_runtime`
- 复用核心库已有的 `recursive::config_file::FileConfig`
- 不动核心库
- 不引入新依赖

## Why

用户报告：
> Error: no LLM provider configured (set OPENAI_API_KEY or RECURSIVE_API_KEY)
> 我以前用旧版 recursive 的 cli config 过了，TUI 不能复用它的配置吗？

调查：
- CLI 通过 `recursive config set provider.api_key ...` 写入 `~/.recursive/config.toml`，
  并在 `src/main.rs:1092` 通过 `recursive::config_file::FileConfig::load()`
  加载它。
- TUI 的 `Backend::build_runtime`（goal 143 时落地，
  `crates/recursive-tui/src/backend.rs:176-218`）**只读环境变量**
  `RECURSIVE_API_KEY` / `OPENAI_API_KEY` / `RECURSIVE_API_BASE` /
  `RECURSIVE_MODEL`，从未读取 config 文件。
- 结果：CLI 用户切换到 TUI 必须额外 `export` 一遍，体验割裂。

这是 Goal 143 的遗漏 —— goal 文件没有提"复用 CLI config"的要求，子
agent 也没主动想到。本 goal 修复这个遗漏。

## Scope (do exactly this, no more)

### 1. `Backend::build_runtime` 的解析顺序升级

修改 `crates/recursive-tui/src/backend.rs::build_runtime`，把当前
"env var → offline" 的两阶段升级为 **"env var → config file → offline"**
三阶段，与 CLI 的 priority chain 对齐（CLI 是
"flag > env > config > default"，TUI 没有 flag，所以是 "env > config > default"）。

具体：

- 在原 `build_runtime` 头部调用一次 `FileConfig::load()`
  - 失败（解析错误）→ 不致命，当作 None 处理（push 一行 warn 到 stderr 或忽略）
  - None（文件不存在）→ 当前行为
  - Some(cfg) → 把 `cfg.provider.{api_key,api_base,model,provider_type}`
    作为 fallback 来源
- 各字段解析顺序：
  - `api_key`：`RECURSIVE_API_KEY` → `OPENAI_API_KEY` → `cfg.provider.api_key`
  - `api_base`：`RECURSIVE_API_BASE` → `OPENAI_API_BASE` → `cfg.provider.api_base` → `"https://api.openai.com/v1"`
  - `model`：`RECURSIVE_MODEL` → `OPENAI_MODEL` → `cfg.provider.model` → `"gpt-4o-mini"`
- 如果三阶段后 `api_key` 仍为 None → 进入 `RuntimeBuild::Offline`，
  reason 文案更新为：
  ```
  "no LLM provider configured. Set OPENAI_API_KEY / RECURSIVE_API_KEY,
   or run `recursive config set provider.api_key ...` to populate
   ~/.recursive/config.toml."
  ```

### 2. URL 规范化

CLI 配置里 `api_base = "https://api.deepseek.com"`（不带 `/v1`），
而 OpenAiProvider 期望 base URL 含路径前缀。看 CLI 怎么处理这个：

- 读 `src/main.rs` 中调 `OpenAiProvider::new` 的位置，确认是否有 `/v1`
  自动追加逻辑
- 如果 CLI 也是裸 base + provider 内部追加 `/v1`，TUI 直接照搬即可
- 如果 CLI 显式拼接 `/v1`，TUI 也要做同样的事

不做"猜测式"补全。读完代码再决定。

### 3. 不读什么

`FileConfig` 里还有 `agent` / `permissions` 等 section，本 goal **不读它们**
—— 只取 `provider`。`AgentRuntimeBuilder` 已有合理默认（max_steps、
temperature、shell_timeout 等），TUI 不暴露这些参数 UI，先不连。
后续 goal 再做。

### 4. 测试

- `backend::build_runtime_uses_config_file_when_env_unset`
  - 用 `tempfile::TempDir` 写一个假 `~/.recursive/config.toml`
  - 设 `HOME=tmpdir`（用 `serial_test` 或同模块 mutex 串行；项目已有
    `test_util::PinnedRecursiveHome` 模式）
  - 清空所有相关 env var
  - 调 `build_runtime()`
  - 断言返回 `RuntimeBuild::Ready` 而不是 Offline
- `backend::env_var_overrides_config_file`
  - 同上但同时 set env，断言 env 优先
- `backend::offline_mode_message_mentions_config_file`
  - 无 env、无 config，断言 reason 包含 "recursive config set"

env-mutating 测试加 `#[ignore]` 标志或者用项目已有的 `test_util::with_pinned_home`，避免和别处的 env 测试打架（`.dev/AGENTS.md` 第 17 课）。

### 5. 不做的事

- ❌ 不在 TUI 里 hot-reload config（启动时读一次即可）
- ❌ 不暴露"切 provider"的 UI（goal 146 `/model` 命令仍是只读显示）
- ❌ 不读 `agent` / `permissions` section
- ❌ 不改 CLI

## Acceptance

1. `cargo build -p recursive-tui` 通过
2. `cargo test --workspace` 全绿
3. `cargo clippy --all-targets --all-features --workspace -- -D warnings` 无警告
4. `cargo fmt --all -- --check` 通过
5. 手工冒烟：
   - 在 `~/.recursive/config.toml` 已存在的环境下，**不设任何 env var**，
     `cargo run -p recursive-tui` 启动后输入消息能成功收到 LLM 回复
   - Status Bar 的 model 字段显示 config 里的 model（如 `deepseek-v4-flash`）

## Notes for the agent

- 关键参考文件：`src/config_file.rs`（FileConfig 定义）、
  `src/main.rs:1092`（CLI 怎么用 FileConfig）、
  `crates/recursive-tui/src/backend.rs:176-218`（待改的位置）
- `recursive_agent::config_file::FileConfig` 已经在 lib 公开（`src/lib.rs:17`），
  TUI 可以直接 `use recursive::config_file::FileConfig;`
- 改动应该是 `build_runtime` 内部的事，**函数签名不变**
- url 规范化：先读 `src/main.rs` 看 CLI 的 OpenAiProvider 构造代码
- 如果 config 文件里 provider_type 不是 "openai"（如 "anthropic"），先按
  openai 走 —— 本 goal 不扩展 provider 选择逻辑。可以在 reason 里 warn
  "provider_type=anthropic not yet supported in TUI; using OpenAI"
- 整个改动应该在 ~50-100 行（含测试）
