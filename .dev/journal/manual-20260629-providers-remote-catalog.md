# Manual edit: providers-remote-catalog

**Date**: 2026-06-29
**Goal**: 在当前 main 上重做"远程 provider catalog + 7 天 TTL 缓存"特性，适配 main 已有的 `providers.d` 架构与严格 `find_preset` 语义。源自历史分支 `feat/providers-remote-update` 的 `e7b7a68`，因 main 演进无法直接 cherry-pick 而重做。
**Files touched**:
- `src/providers_cache.rs` (新增) — 远程 catalog 下载/缓存/合并，含 SSRF 校验
- `src/providers.rs` — 加 `Serialize` derive；新增 `all_presets_effective` / `find_preset_effective` / `find_model_pricing_effective`（remote cache > bundled > providers.d）
- `src/lib.rs` — 注册 `providers_cache` 模块，导出 `all_presets_effective` / `find_preset_effective`
- `src/llm/pricing.rs` — `pricing_for` 切到 `find_model_pricing_effective`，让定价反映上游刷新
- `crates/recursive-cli/src/main.rs` — 新增 `Providers { Update|List|Status }` 子命令；启动时调用 `spawn_background_refresh`（仅 `RECURSIVE_PROVIDERS_AUTO_REFRESH=1` 且缓存过期时生效）
**Tests added**:
- `providers_cache::tests::merge_*`（3）、`validate_url_*`（3）、`needs_update_true_when_no_cache`、`load_cache_returns_none_when_absent`、`fetch_roundtrips_through_cache_file`
**Notes**:
- 刻意保留 `find_preset` / `find_preset_by_api_base` / `find_model_pricing` 的 bundled-only 严格语义（main 的设计选择，`Config::from_env` 依赖），新增 effective 系列而非改写既有函数
- SSRF 校验镜像 `tools::web_fetch` 的实现（非共享，避免扩大改动半径）
- 后台刷新默认关闭（env 门控），一次性命令不会发惊喜网络请求
- 远端 repo `jeffkit/recursive-providers` 实测可拉取（12 个 preset）
- 质量门：`cargo test --workspace` / `cargo clippy --all-targets --all-features -- -D warnings` / `cargo fmt --all` 全绿
