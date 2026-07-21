# Manual edit: minimax-cn preset (国内站端点)

**Date**: 2026-07-20
**Goal**: 修复 TUI `/model` 切到内置 minimax 后聊天报 `invalid api key`
的问题。根因是内置 minimax preset 指向国际站 `api.minimax.io`，而国内
用户的 `MINIMAX_API_KEY` 是国内站 `api.minimaxi.com` 的 key，两套平台
独立发 key、不通用，国际站原样回 401 `invalid api key (2049)`。

**Files touched**:
- `providers.toml` — 新增 `minimax-cn` preset（api_base =
  `https://api.minimaxi.com/v1`，anthropic_api_base =
  `https://api.minimaxi.com/anthropic`，key_env 仍为 `MINIMAX_API_KEY`）。
  保留原 `minimax`（国际站）不动，两套 key 的用户互不影响。
- `src/providers.rs` — 新增测试
  `find_preset_minimax_cn_uses_domestic_endpoint` 钉住国内站端点。

**Tests added**:
- `providers::tests::find_preset_minimax_cn_uses_domestic_endpoint`

**Notes**:
- 实测确认：同一把 `MINIMAX_API_KEY` 打国际站 `/v1/models` → 401
  `invalid api key (2049)`；打国内站 `/v1/models` → 200，`/v1/chat/completions`
  正常返回 completion，`/anthropic` 端点也 200。
- `providers.d/` 无法覆盖内置 id（`src/providers.rs` 的
  `compute_effective_presets` 只在 id 不存在时才追加），所以用户侧
  没法靠 providers.d 修，必须改内置 catalog。
- `build_provider_for_model`（`/model` 热切换路径）的 `api_base` 完全
  取自 preset，不读 `RECURSIVE_API_BASE` / config 文件 `api_base`，
  所以 env 覆盖对热切换不生效——这也是为什么必须改 preset 本身。
- 两个 preset 共用 `key_env = "MINIMAX_API_KEY"`：picker 里两个都会
  出现（`preset_key_available` 只看 env 是否非空），用户按自己 key
  的平台选对应的那个。
- providers.toml 是 `include_str!` 编译期内嵌，用户需要重新
  `cargo build` / 重装 binary 才能在 TUI 里看到新 preset。

**Gates**: `cargo test --workspace` ✅, `cargo clippy --all-targets
--all-features -- -D warnings` ✅, `cargo fmt --all --check` ✅.
